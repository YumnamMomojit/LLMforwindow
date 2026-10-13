//! Local-owner commands: workspaces, chat history settings, database backup, and a terminal
//! chat client for the local API. None of this is reachable through the gateway.
use crate::{usage_error, Args};
use anyhow::{anyhow, bail, Context, Result};
use fs4::fs_std::FileExt;
use futures_util::{FutureExt, StreamExt};
use semdok_core::storage::{WorkspaceInfo, Workspaces};
use semdok_core::{db, history};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Writes stop rather than leave less than this free on the workspace volume.
const RESERVE_FREE_BYTES: u64 = 512 << 20;
const MAX_QUOTA_MB: i64 = 16 << 20; // 16 TiB

/// The workspace store, with the workspace lock held until the returned file is dropped.
/// Opening the store clears every workspace's interrupted uploads, so without the lock a
/// `workspace list` in a second window deletes the upload a running `workspace put` is still
/// writing, and that put then fails.
fn workspaces(dir: &Path) -> Result<(Workspaces, std::fs::File)> {
    let c = db::open(&dir.join("hub.db"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("workspaces.lock"))
        .context("cannot open the workspace lock file")?;
    if !lock.try_lock_exclusive()? {
        eprintln!("waiting for another workspace or backup command to finish ...");
        lock.lock_exclusive()?;
    }
    let w = Workspaces::new(c, &dir.join("workspaces"), RESERVE_FREE_BYTES)?;
    Ok((w, lock))
}

/// A workspace by exact id, else by exact name.
fn find(w: &Workspaces, which: &str) -> Result<WorkspaceInfo> {
    let all = w.list_workspaces()?;
    all.iter()
        .find(|i| i.id == which)
        .or_else(|| all.iter().find(|i| i.name == which))
        .cloned()
        .with_context(|| {
            format!("no workspace named or with id '{which}' (semdok-edge workspace list)")
        })
}

fn mb(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.1} KB", b as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", b as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", b as f64 / 1_073_741_824.0),
    }
}

pub fn workspace(dir: &Path, mut args: Args) -> Result<()> {
    let sub =
        args.word("workspace subcommand (list, create, files, put, get, rm, delete, export)")?;
    match sub.as_str() {
        "list" => {
            args.finish()?;
            let (w, _lock) = workspaces(dir)?;
            let all = w.list_workspaces()?;
            if all.is_empty() {
                println!("no workspaces yet: semdok-edge workspace create <name>");
            }
            for i in all {
                let quota = i.quota_bytes.map_or("no quota".into(), mb);
                println!(
                    "{}\t{}\t{} files\t{} used\t{}",
                    i.id,
                    i.name,
                    i.files,
                    mb(i.used_bytes),
                    quota
                );
            }
        }
        "create" => {
            let quota = args
                .value("--quota-mb")?
                .map(|v| crate::bounded("--quota-mb", &v, 1, MAX_QUOTA_MB))
                .transpose()?;
            let name = args.word("workspace name")?;
            args.finish()?;
            let (w, _lock) = workspaces(dir)?;
            let i = w.create(&name, quota.map(|q| q as u64 * (1 << 20)))?;
            println!("created workspace {} ({})", i.name, i.id);
        }
        "files" => {
            let which = args.word("workspace")?;
            args.finish()?;
            let (w, _lock) = workspaces(dir)?;
            let i = find(&w, &which)?;
            for f in w.list_files(&i.id)? {
                println!(
                    "{}\t{}\t{}\tsha256:{}",
                    f.rel_path,
                    mb(f.size),
                    f.kind,
                    f.sha256
                );
            }
        }
        "put" => {
            let as_rel = args.value("--as")?;
            let overwrite = args.switch("--overwrite");
            let which = args.word("workspace")?;
            let src = PathBuf::from(args.word("file to import")?);
            args.finish()?;
            let rel = match as_rel {
                Some(r) => r,
                None => src
                    .file_name()
                    .context("the source path has no file name; pass --as NAME")?
                    .to_string_lossy()
                    .into_owned(),
            };
            // Checked before opening: opening a FIFO would wait forever for a writer.
            let meta = std::fs::metadata(&src)
                .with_context(|| format!("cannot open {}", src.display()))?;
            if !meta.is_file() {
                bail!("{} is not a regular file", src.display());
            }
            let mut f = std::fs::File::open(&src)
                .with_context(|| format!("cannot open {}", src.display()))?;
            if !f.metadata()?.is_file() {
                bail!("{} is not a regular file", src.display());
            }
            let (mut w, _lock) = workspaces(dir)?;
            let i = find(&w, &which)?;
            // Stored as data: never extracted, never executed.
            let r = w.write_file(&i.id, &rel, &mut f, "import", overwrite)?;
            println!(
                "stored {} in {} ({}, sha256:{})",
                r.rel_path,
                i.name,
                mb(r.size),
                r.sha256
            );
        }
        "get" => {
            let overwrite = args.switch("--overwrite");
            let which = args.word("workspace")?;
            let rel = args.word("file in the workspace")?;
            let dest = PathBuf::from(args.word("destination file")?);
            args.finish()?;
            let (w, _lock) = workspaces(dir)?;
            let i = find(&w, &which)?;
            let mut src = w.read_file(&i.id, &rel)?;
            let mut out = std::fs::OpenOptions::new();
            out.write(true);
            if overwrite {
                out.create(true).truncate(true);
            } else {
                out.create_new(true);
            }
            let mut out = out.open(&dest).with_context(|| {
                format!("cannot write {} (exists? use --overwrite)", dest.display())
            })?;
            // A FIFO, a terminal or a device (`/dev/stdout`, `| less`) is written as a stream:
            // fsync means nothing there (it fails with EINVAL), and the path is never ours to
            // remove.
            let regular = out.metadata().is_ok_and(|m| m.is_file());
            let copied = std::io::copy(&mut src, &mut out).and_then(|n| {
                if regular {
                    out.sync_all()?;
                }
                Ok(n)
            });
            let n = match copied {
                Ok(n) => n,
                Err(e) => {
                    let what = if regular {
                        discard_partial(&dest, out)
                    } else {
                        ""
                    };
                    return Err(e)
                        .with_context(|| format!("writing {} failed{what}", dest.display()));
                }
            };
            // Always on stderr: the destination can be stdout itself even when it is a regular
            // file (`get ws f /dev/stdout > out.txt`), and a line on stdout would land in the copy.
            eprintln!("wrote {} ({})", dest.display(), mb(n));
        }
        "rm" => {
            let yes = args.switch("--yes");
            let which = args.word("workspace")?;
            let rel = args.word("file in the workspace")?;
            args.finish()?;
            if !yes {
                bail!("deleting '{rel}' cannot be undone; repeat with --yes");
            }
            let (w, _lock) = workspaces(dir)?;
            let i = find(&w, &which)?;
            w.delete_file(&i.id, &rel, true)?;
            println!("deleted {rel} from {}", i.name);
        }
        "delete" => {
            let yes = args.switch("--yes");
            let which = args.word("workspace")?;
            args.finish()?;
            let (w, _lock) = workspaces(dir)?;
            let i = find(&w, &which)?;
            if !yes {
                bail!(
                    "deleting workspace '{}' removes its {} files ({}) and cannot be undone; repeat with --yes",
                    i.name, i.files, mb(i.used_bytes)
                );
            }
            w.delete_workspace(&i.id, true)?;
            println!("deleted workspace {} (not a secure erase)", i.name);
        }
        "export" => {
            let overwrite = args.switch("--overwrite");
            let which = args.word("workspace")?;
            let dest = PathBuf::from(args.word("destination .zip")?);
            args.finish()?;
            // The export is renamed into place at the end, which would silently replace a file.
            if !overwrite && std::fs::symlink_metadata(&dest).is_ok() {
                bail!(
                    "{} already exists; choose another name or add --overwrite",
                    dest.display()
                );
            }
            let (w, _lock) = workspaces(dir)?;
            let i = find(&w, &which)?;
            w.export_workspace(&i.id, &dest)?;
            println!(
                "exported {} ({} files) to {}",
                i.name,
                i.files,
                dest.display()
            );
        }
        _ => return Err(usage_error(format!("unknown workspace subcommand '{sub}'"))),
    }
    Ok(())
}

/// After a failed copy into the regular file `out` opened at `dest`: never leave a cut-off
/// copy that looks complete. The path is removed only when it names that very file; through
/// a symlink (or if the path was replaced meanwhile) the file is emptied instead, so the link
/// and whatever the path now names stay. Returns what was done, for the error message.
fn discard_partial(dest: &Path, out: std::fs::File) -> &'static str {
    let is_dest = match (out.metadata(), std::fs::symlink_metadata(dest)) {
        (Ok(opened), Ok(at_path)) => at_path.is_file() && same_file(&opened, &at_path),
        _ => false,
    };
    // Emptied first, so a removal that fails still leaves nothing that looks complete.
    let emptied = out.set_len(0).is_ok();
    drop(out); // Windows cannot remove a file that is still open
    if is_dest && std::fs::remove_file(dest).is_ok() {
        "; the partial copy was removed"
    } else if emptied {
        "; the partial copy was emptied"
    } else {
        ""
    }
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// No stable file id on Windows: `symlink_metadata` being a regular file (not a link) is the
/// check that remains.
#[cfg(windows)]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

pub fn backup(dir: &Path, mut args: Args) -> Result<()> {
    let dest = PathBuf::from(args.word("destination file")?);
    args.finish()?;
    let (w, _lock) = workspaces(dir)?;
    w.backup_database(&dest)?;
    println!(
        "database snapshot written to {} (API keys and request log removed; workspace files, \
         models and secrets are not included: use `workspace export`)",
        dest.display()
    );
    Ok(())
}

pub fn history_cmd(dir: &Path, mut args: Args) -> Result<()> {
    let sub = args.word("history subcommand (status, on, off, retention, list, show, delete)")?;
    // Opened only after the arm has taken all of its arguments, like every other command.
    let open = || db::open(&dir.join("hub.db"));
    match sub.as_str() {
        "status" => {
            args.finish()?;
            let c = open()?;
            println!(
                "history: {}; retention: {}",
                if history::enabled(&c) {
                    "on"
                } else {
                    "off (nothing new is stored)"
                },
                history::retention_days(&c)
                    .map_or("keep until deleted".into(), |d| format!("{d} days"))
            );
        }
        "on" | "off" => {
            args.finish()?;
            history::set_enabled(&open()?, sub == "on")?;
            println!("history {sub}");
        }
        "retention" => {
            let v = args.word("days, or 'none'")?;
            args.finish()?;
            let days = if v == "none" {
                None
            } else {
                Some(crate::bounded("retention", &v, 1, 36_500)?)
            };
            let c = open()?;
            history::set_retention_days(&c, days)?;
            println!(
                "retention set; removed now: {} conversations",
                history::purge_expired(&c)?
            );
        }
        "list" => {
            args.finish()?;
            for v in history::list_conversations(&open()?)? {
                // Titles are the first line typed (or pasted) into a chat: shown inert.
                println!(
                    "{}\t{}\t{} messages\tupdated {}\t{}",
                    v.id,
                    clean_line(&v.model.unwrap_or_default()),
                    v.messages,
                    v.updated_at,
                    clean_line(&v.title)
                );
            }
        }
        "show" => {
            let id = args.word("conversation id")?;
            args.finish()?;
            let c = open()?;
            if !history::list_conversations(&c)?.iter().any(|v| v.id == id) {
                bail!(
                    "no conversation with id '{}' (semdok-edge history list)",
                    clean_line(&id)
                );
            }
            print!("{}", clean(&history::export_text(&c, &id)?));
        }
        "delete" => {
            let yes = args.switch("--yes");
            let id = args.word("conversation id")?;
            args.finish()?;
            if !yes {
                bail!("deleting conversation {id} cannot be undone; repeat with --yes");
            }
            println!(
                "{}",
                if history::delete_conversation(&open()?, &id)? {
                    "deleted"
                } else {
                    "no such conversation"
                }
            );
        }
        _ => return Err(usage_error(format!("unknown history subcommand '{sub}'"))),
    }
    Ok(())
}

/// Model output is inert text: drop terminal control sequences (ESC, other C0/C1 controls)
/// so a reply cannot move the cursor, retitle the window or hide text. Keeps \n and \t.
pub(crate) fn clean(s: &str) -> String {
    s.chars()
        .filter(|&ch| ch == '\n' || ch == '\t' || !(ch.is_control()))
        .collect()
}

/// `clean` for one field of a one-line, tab-separated listing.
fn clean_line(s: &str) -> String {
    clean(s).replace(['\n', '\t'], " ")
}

/// Ctrl-C for the interactive commands: one listener for the whole command, so a press is
/// never lost between a reply and the next prompt. While it exists Ctrl-C no longer ends the
/// process by itself; the command decides what it means and puts the terminal back first.
pub(crate) struct Interrupt {
    #[cfg(unix)]
    s: tokio::signal::unix::Signal,
    #[cfg(windows)]
    s: tokio::signal::windows::CtrlC,
}

impl Interrupt {
    pub(crate) fn new() -> Result<Self> {
        #[cfg(unix)]
        let s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        #[cfg(windows)]
        let s = tokio::signal::windows::ctrl_c()?;
        Ok(Self { s })
    }

    pub(crate) async fn recv(&mut self) {
        self.s.recv().await;
    }

    /// Forgets presses that arrived while nothing was waiting for one (e.g. just after a
    /// reply ended), so they do not end the chat at the next prompt.
    fn clear(&mut self) {
        while let Some(Some(())) = self.s.recv().now_or_never() {}
    }
}

/// Terminal echo off while it lives; the saved mode is put back on drop. Also put back when
/// the process is ended meanwhile by a signal or console event it does not handle itself
/// (SIGTERM, SIGQUIT, SIGHUP; Ctrl-Break, closing the console), whose default action would
/// otherwise leave the shell with a terminal that shows nothing typed. Ctrl-C is not one of
/// them: `Interrupt` turns it into an ordinary event, and the guard is dropped as usual.
/// One guard at a time (a prompt is open at most once at a time).
struct NoEcho {
    #[cfg(unix)]
    saved: libc::termios,
    /// Signals given `restore_echo_and_reraise`, with the action each had before.
    #[cfg(unix)]
    handlers: Vec<(libc::c_int, libc::sigaction)>,
    #[cfg(windows)]
    saved: (windows_sys::Win32::Foundation::HANDLE, u32),
}

/// The terminal mode a signal handler puts back: written before the handlers are installed,
/// read only by them.
#[cfg(unix)]
struct SavedTermios(std::cell::UnsafeCell<std::mem::MaybeUninit<libc::termios>>);
// SAFETY: written only while no handler that reads it is installed (see `NoEcho::new`).
#[cfg(unix)]
unsafe impl Sync for SavedTermios {}
#[cfg(unix)]
static SAVED_TERMIOS: SavedTermios =
    SavedTermios(std::cell::UnsafeCell::new(std::mem::MaybeUninit::uninit()));

/// Only async-signal-safe calls (tcsetattr, raise). SA_RESETHAND has already put the default
/// action back, so the raised signal ends the process as it would have without this handler
/// (same exit status, core dump for SIGQUIT) once the handler returns.
#[cfg(unix)]
extern "C" fn restore_echo_and_reraise(sig: libc::c_int) {
    // SAFETY: SAVED_TERMIOS was initialised before this handler was installed.
    unsafe {
        libc::tcsetattr(
            libc::STDIN_FILENO,
            libc::TCSANOW,
            (*SAVED_TERMIOS.0.get()).as_ptr(),
        );
        libc::raise(sig);
    }
}

#[cfg(windows)]
static SAVED_CONSOLE: (
    std::sync::atomic::AtomicPtr<std::ffi::c_void>,
    std::sync::atomic::AtomicU32,
) = (
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
    std::sync::atomic::AtomicU32::new(0),
);

/// Runs on its own thread for every console event while a prompt is open (handlers run newest
/// first). Puts the console mode back for the events that end the process, then lets the next
/// handler decide: Ctrl-C goes on to `Interrupt`, the rest to the default (exit).
#[cfg(windows)]
unsafe extern "system" fn restore_console_on_exit(
    event: u32,
) -> windows_sys::Win32::Foundation::BOOL {
    use std::sync::atomic::Ordering::SeqCst;
    use windows_sys::Win32::System::Console::{SetConsoleMode, CTRL_C_EVENT};
    let h = SAVED_CONSOLE.0.load(SeqCst);
    if event != CTRL_C_EVENT && !h.is_null() {
        SetConsoleMode(h, SAVED_CONSOLE.1.load(SeqCst));
    }
    0
}

impl NoEcho {
    #[cfg(unix)]
    fn new() -> Option<Self> {
        // SAFETY: termios calls on stdin with a struct that tcgetattr fills in; sigaction with
        // zeroed structs filled in here. No handler is installed while SAVED_TERMIOS is written.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return None;
            }
            let saved = t;
            (*SAVED_TERMIOS.0.get()).write(saved);
            let mut handlers = vec![];
            for sig in [libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP] {
                // Only where the default action applies: an ignored SIGHUP (nohup) stays
                // ignored, and a handler someone else installed stays theirs.
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut old) != 0
                    || old.sa_sigaction != libc::SIG_DFL
                {
                    continue;
                }
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction =
                    restore_echo_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t;
                new.sa_flags = libc::SA_RESETHAND;
                libc::sigemptyset(&mut new.sa_mask);
                // tcsetattr from a background job would raise SIGTTOU and stop the process
                // inside the handler; with SIGTTOU blocked it is allowed (POSIX) and the
                // re-raised signal then ends the process as it should.
                libc::sigaddset(&mut new.sa_mask, libc::SIGTTOU);
                if libc::sigaction(sig, &new, std::ptr::null_mut()) == 0 {
                    handlers.push((sig, old));
                }
            }
            let guard = Self { saved, handlers };
            t.c_lflag &= !libc::ECHO;
            // On failure the guard's drop puts the handlers back.
            (libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) == 0).then_some(guard)
        }
    }

    #[cfg(windows)]
    fn new() -> Option<Self> {
        use std::sync::atomic::Ordering::SeqCst;
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleCtrlHandler, SetConsoleMode, ENABLE_ECHO_INPUT,
            STD_INPUT_HANDLE,
        };
        // SAFETY: console mode calls on this process's own stdin handle; the handler routine
        // only reads the two atomics.
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(h, &mut mode) == 0 {
                return None;
            }
            SAVED_CONSOLE.1.store(mode, SeqCst);
            SAVED_CONSOLE.0.store(h, SeqCst);
            SetConsoleCtrlHandler(Some(restore_console_on_exit), 1);
            let guard = Self { saved: (h, mode) };
            // On failure the guard's drop removes the handler again.
            (SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) != 0).then_some(guard)
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restores the mode read in `new` on the same handle, then the signal actions
        // (console handler) that `new` replaced (added). A signal in between finds the mode
        // already restored and ends the process as it would have anyway.
        #[cfg(unix)]
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
            for (sig, old) in &self.handlers {
                libc::sigaction(*sig, old, std::ptr::null_mut());
            }
        }
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, SetConsoleMode};
            SetConsoleMode(self.saved.0, self.saved.1);
            SetConsoleCtrlHandler(Some(restore_console_on_exit), 0);
            SAVED_CONSOLE
                .0
                .store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// What to show before reading a secret at a terminal.
pub(crate) struct SecretPrompt {
    /// When the typing is hidden.
    hidden: &'static str,
    /// When it cannot be: e.g. Git Bash's mintty without a pseudo console, whose pty reaches a
    /// Windows program as a pipe that still counts as a terminal but has no console mode.
    visible: &'static str,
}

pub(crate) const TUNNEL_TOKEN_PROMPT: SecretPrompt = SecretPrompt {
    hidden: "tunnel token (typing is hidden): ",
    visible: "warning: this terminal cannot hide what you type, so the token will be shown \
              on screen and kept in the scrollback. To avoid that, press Ctrl-C and use \
              `semdok-edge secret set tunnel_token < token.txt` (or run it from cmd or PowerShell).\n\
              tunnel token (VISIBLE as you type): ",
};

const CHAT_KEY_PROMPT: SecretPrompt = SecretPrompt {
    hidden: "API key (sem_...; typing is hidden; set SEMDOK_API_KEY to skip this): ",
    visible: "warning: this terminal cannot hide what you type, so the key will be shown on \
              screen and kept in the scrollback. To avoid that, press Ctrl-C and set \
              SEMDOK_API_KEY instead (or run the chat from cmd or PowerShell).\n\
              API key (sem_...; VISIBLE as you type): ",
};

/// One line from stdin for a key or token, without its line ending. At a terminal the prompt
/// goes to stderr and the typing is not shown, so the secret stays off the screen and out of
/// the scrollback (or, where the terminal cannot hide it, the prompt says so); from a pipe the
/// line is read as it comes, with no prompt. Ctrl-C cancels either way (and puts the terminal
/// back first).
pub(crate) async fn read_secret_line(
    prompt: &SecretPrompt,
    intr: &mut Interrupt,
) -> Result<String> {
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal();
    let echo = if tty { NoEcho::new() } else { None };
    if tty {
        eprint!(
            "{}",
            if echo.is_some() {
                prompt.hidden
            } else {
                prompt.visible
            }
        );
        std::io::stderr().flush()?;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A thread, not the runtime: a blocked read must not keep Ctrl-C from being handled.
    std::thread::spawn(move || {
        let mut v = String::new();
        let _ = tx.send(std::io::stdin().lock().read_line(&mut v).map(|_| v));
    });
    intr.clear();
    let line = tokio::select! {
        r = rx => match r {
            Ok(r) => r.context("cannot read stdin"),
            Err(_) => Err(anyhow!("cannot read stdin")),
        },
        _ = intr.recv() => Err(anyhow!("cancelled; nothing was stored or sent")),
    };
    drop(echo);
    if tty {
        eprintln!();
    }
    line.map(|v| v.trim_end_matches(['\r', '\n']).to_string())
}

/// Lines from stdin, read on their own thread so that the prompt can wait for a line and for
/// Ctrl-C at once. The channel closes at end of input.
fn stdin_lines() -> tokio::sync::mpsc::Receiver<std::io::Result<String>> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || loop {
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) => break,
            r => {
                let failed = r.is_err();
                if tx.blocking_send(r.map(|_| line)).is_err() || failed {
                    break;
                }
            }
        }
    });
    rx
}

struct ChatOpts {
    url: String,
    model: Option<String>,
    system: String,
    max_tokens: Option<i64>,
    no_history: bool,
}

/// Parsed, not pattern-matched: `http://127.0.0.1:80@evil.com/` has host evil.com.
fn loopback_http(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    u.scheme() == "http"
        && u.username().is_empty()
        && u.password().is_none()
        && match u.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => false,
        }
}

/// What a chat line asks for.
#[derive(Debug, PartialEq)]
enum ChatInput<'a> {
    Skip,
    Exit,
    New,
    System(&'a str),
    Help,
    Message,
}

fn chat_input(line: &str) -> ChatInput<'_> {
    let t = line.trim();
    let (cmd, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    match cmd {
        "" => ChatInput::Skip,
        "/exit" | "/quit" if rest.is_empty() => ChatInput::Exit,
        "/new" if rest.is_empty() => ChatInput::New,
        "/system" => ChatInput::System(rest.trim()),
        _ if t.starts_with('/') => ChatInput::Help,
        _ => ChatInput::Message,
    }
}

/// Longest wait for `GET /models`, which the local API answers at once.
const MODELS_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for a whole reply. The local API ends a stream after 15 minutes (and sends
/// keepalives while the model reads the prompt), so this only bounds a server that hangs.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3600);

/// `work`, unless Ctrl-C comes first.
async fn or_ctrl_c<T>(
    intr: &mut Interrupt,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        r = work => r,
        _ = intr.recv() => Err(anyhow!("cancelled")),
    }
}

/// Blocking work on a thread of its own, so that the runtime (and Ctrl-C) stays responsive
/// meanwhile. Not `spawn_blocking`: the runtime waits for those at exit, so a cancelled wait
/// would still hold the process.
fn on_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> impl std::future::Future<Output = Result<T>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    async move {
        rx.await
            .map_err(|_| anyhow!("internal error: a worker thread failed"))
    }
}

/// A failed request to the local API, worded for what most likely happened.
fn unreachable_api(base: &str, wait: Duration, e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() && !e.is_connect() {
        anyhow!(
            "{base} accepted the connection but did not answer within {} s: is `semdok-edge serve` \
             suspended (Ctrl-Z) or stuck?",
            wait.as_secs()
        )
    } else {
        anyhow::Error::new(e).context(format!(
            "cannot reach {base}: is `semdok-edge serve` running?"
        ))
    }
}

/// The id of the first model the server lists.
async fn first_model(http: &reqwest::Client, base: &str, key: &str) -> Result<String> {
    let fail = |e| unreachable_api(base, MODELS_TIMEOUT, e);
    let r = http
        .get(format!("{base}/models"))
        .bearer_auth(key)
        .timeout(MODELS_TIMEOUT)
        .send()
        .await
        .map_err(fail)?;
    let st = r.status();
    let v: Value = serde_json::from_slice(&r.bytes().await.map_err(fail)?).unwrap_or(Value::Null);
    if !st.is_success() {
        bail!(
            "{st}: {}",
            clean_line(v["error"]["message"].as_str().unwrap_or("request failed"))
        );
    }
    Ok(v["data"][0]["id"]
        .as_str()
        .context("the server lists no model")?
        .to_string())
}

/// What a reply that ended at its token limit (`finish_reason: "length"`) gets said about it.
/// `completion_tokens` below the `--max-tokens` asked for means the server lowered the limit.
fn length_notes(asked: Option<i64>, completion_tokens: Option<i64>) -> Vec<String> {
    let mut notes = vec![
        "[reply stopped at the token limit; use --max-tokens N, up to the server limit]"
            .to_string(),
    ];
    if let (Some(asked), Some(got)) = (asked, completion_tokens) {
        if (1..asked).contains(&got) {
            // The client cannot tell which limit it was: the server's cap, or the engine's
            // context window (the engine shrinks the budget to the room the prompt leaves).
            notes.push(format!(
                "[the reply stopped after {got} of the {asked} tokens asked for: either the \
                 server's limit (`semdok-edge serve --max-output-tokens N`) or the model's context \
                 window, which a long conversation fills (/new starts fresh)]"
            ));
        }
    }
    notes
}

pub async fn chat(dir: &Path, mut args: Args) -> Result<()> {
    let o = ChatOpts {
        url: args
            .value("--url")?
            .unwrap_or_else(|| "http://127.0.0.1:8787/v1".into()),
        model: args.value("--model")?,
        system: args.value("--system")?.unwrap_or_default(),
        max_tokens: args
            .value("--max-tokens")?
            .map(|v| crate::bounded("--max-tokens", &v, 1, 1_000_000))
            .transpose()?,
        no_history: args.switch("--no-history"),
    };
    args.finish()?;
    // Local chat only: a key must not travel over plain HTTP to another machine.
    if !loopback_http(&o.url) {
        bail!("semdok-edge chat talks to the local API only (http://127.0.0.1:<port>/v1); for a remote endpoint use the examples/");
    }
    let base = o.url.trim_end_matches('/').to_string();
    // From here on Ctrl-C is an event, not the end of the process: every wait below races it.
    let mut intr = Interrupt::new()?;
    let key = match crate::env_os("SEMDOK_API_KEY")
        .and_then(|k| k.into_string().ok())
        .filter(|k| !k.trim().is_empty())
    {
        Some(k) => k,
        None => read_secret_line(&CHAT_KEY_PROMPT, &mut intr).await?,
    };
    let key = key.trim().to_string();
    if !key.starts_with("sem_") && !key.starts_with("lah_") {
        bail!("that is not a Semdok Edge key (they start with sem_); create one with `semdok-edge key create <name>`");
    }
    if !key.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("the key contains spaces or control characters; copy it again");
    }
    let http = reqwest::Client::builder()
        .no_proxy()
        // The local API never redirects; following one could carry the conversation elsewhere.
        .redirect(reqwest::redirect::Policy::none())
        // `localhost` always means this machine, whatever a hosts file or resolver says.
        .resolve_to_addrs(
            "localhost",
            &[
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
            ],
        )
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let model = match o.model.clone() {
        Some(m) => m,
        None => or_ctrl_c(&mut intr, first_model(&http, &base, &key)).await?,
    };
    // History is optional: without a usable database the chat still works, unsaved. Opened on
    // its own thread: a database another process holds locked can take seconds.
    let mut db_conn = if o.no_history {
        None
    } else {
        let path = dir.join("hub.db");
        let opened = on_thread(move || {
            db::open(&path).map(|c| {
                let on = history::enabled(&c);
                (c, on)
            })
        });
        match or_ctrl_c(&mut intr, opened).await? {
            Ok((c, true)) => Some(c),
            Ok((_, false)) => None,
            Err(e) => {
                eprintln!("note: chat history cannot be saved: {e:#}");
                None
            }
        }
    };
    let mut conv: Option<String> = None;
    let mut msgs: Vec<Value> = vec![];
    let mut system = o.system.clone();
    let reset = |system: &str, msgs: &mut Vec<Value>| {
        msgs.clear();
        if !system.is_empty() {
            msgs.push(json!({"role": "system", "content": system}));
        }
    };
    reset(&system, &mut msgs);
    println!(
        "chatting with {} at {base}. History: {}. Commands: /new, /system TEXT, /exit. Ctrl-C stops a reply; at the prompt it ends the chat.",
        clean_line(&model),
        if db_conn.is_some() { "saved locally (semdok-edge history off to stop)" } else { "not saved" }
    );
    let mut lines = stdin_lines();
    loop {
        intr.clear();
        print!("\n> ");
        std::io::stdout().flush()?;
        let line = tokio::select! {
            l = lines.recv() => match l {
                Some(l) => l?,
                None => break, // end of input
            },
            _ = intr.recv() => {
                println!();
                break;
            }
        };
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        match chat_input(&line) {
            ChatInput::Skip => continue,
            ChatInput::Exit => break,
            ChatInput::New => {
                conv = None;
                reset(&system, &mut msgs);
                println!("(new conversation)");
                continue;
            }
            ChatInput::System(t) => {
                system = t.to_string();
                conv = None;
                reset(&system, &mut msgs);
                println!(
                    "({}; new conversation)",
                    if system.is_empty() {
                        "system prompt cleared"
                    } else {
                        "system prompt set"
                    }
                );
                continue;
            }
            ChatInput::Help => {
                println!("commands: /new, /system TEXT, /exit");
                continue;
            }
            ChatInput::Message => {}
        }
        msgs.push(json!({"role": "user", "content": line}));
        // Usage too: with `finish_reason: "length"` it shows whether the server lowered the
        // limit asked for.
        let mut body = json!({
            "model": model,
            "messages": msgs,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if let Some(m) = o.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let reply = tokio::select! {
            r = stream_reply(&http, &base, &key, &body) => r,
            _ = intr.recv() => Err(anyhow!("stopped")),
        };
        match reply {
            Ok(Reply {
                text,
                finish,
                completion_tokens,
            }) => {
                if finish.as_deref() == Some("length") {
                    for note in length_notes(o.max_tokens, completion_tokens) {
                        println!("{note}");
                    }
                }
                msgs.push(json!({"role": "assistant", "content": text}));
                // Saved per answered turn; the conversation row comes with the first one, so
                // a chat whose requests all fail leaves no empty conversation behind.
                if let Some(c) = db_conn.as_mut() {
                    let saved = (|| -> Result<()> {
                        if conv.is_none() {
                            let title: String = line.chars().take(60).collect();
                            conv = history::create_conversation(c, &title, &system, Some(&model))?;
                            if let (Some(id), false) = (&conv, system.is_empty()) {
                                history::append(c, id, "system", &system)?;
                            }
                        }
                        if let Some(id) = &conv {
                            history::append(c, id, "user", &line)?;
                            history::append(c, id, "assistant", &text)?;
                        }
                        Ok(())
                    })();
                    if let Err(e) = saved {
                        println!(
                            "[this turn was not saved to history: {}]",
                            clean_line(&format!("{e:#}"))
                        );
                    }
                }
            }
            Err(e) => {
                msgs.pop(); // the unanswered turn is not part of the conversation
                let e = clean(&format!("{e:#}"));
                println!("\n[{e}]");
                if e.contains("context_length_exceeded") {
                    println!(
                        "[the conversation no longer fits the model's context: /new starts over]"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Server-sent events as the HTML standard parses them: lines end in CRLF, LF or CR (also
/// when a CRLF is split between two reads); `data:` lines, with or without one space after
/// the colon, are joined with '\n'; a blank line ends the event; `:` lines are comments.
#[derive(Default)]
struct SseParser {
    /// The current line, up to its end.
    buf: Vec<u8>,
    data: Option<String>,
    /// The last line ended in CR: a LF right after it (maybe in the next read) belongs to it.
    after_cr: bool,
}

/// One event, or one line still waiting for its end, may not grow past this.
const MAX_SSE_EVENT: usize = 16 << 20;

impl SseParser {
    /// Feeds bytes in; returns the data of each event they complete (events without data
    /// are skipped).
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        let mut events = vec![];
        for &b in bytes {
            if std::mem::take(&mut self.after_cr) && b == b'\n' {
                continue;
            }
            if b == b'\n' || b == b'\r' {
                self.after_cr = b == b'\r';
                // Line ends are ASCII, so a line never splits a UTF-8 sequence.
                let line = String::from_utf8_lossy(&self.buf).into_owned();
                self.buf.clear();
                self.line(&line, &mut events);
            } else {
                self.buf.push(b);
            }
        }
        if self.buf.len() + self.data.as_ref().map_or(0, String::len) > MAX_SSE_EVENT {
            bail!("a stream event is larger than {} MiB", MAX_SSE_EVENT >> 20);
        }
        Ok(events)
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if let Some(d) = self.data.take().filter(|d| !d.is_empty()) {
                events.push(d);
            }
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some(("", _)) => return, // comment, e.g. ": keepalive"
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        if field == "data" {
            match &mut self.data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            }
        }
    }
}

/// One streamed reply.
struct Reply {
    text: String,
    /// The last `finish_reason` the server sent: "stop", "length", ...
    finish: Option<String>,
    /// From the usage event, if the server sent one.
    completion_tokens: Option<i64>,
}

/// Streams one reply to stdout and returns it. Thinking (`reasoning_content`) is shown on
/// stderr and not kept in the conversation. Everything the server sends is shown inert.
async fn stream_reply(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    body: &Value,
) -> Result<Reply> {
    let r = http
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key)
        .json(body)
        .timeout(REPLY_TIMEOUT)
        .send()
        .await
        .map_err(|e| unreachable_api(base, REPLY_TIMEOUT, e))?;
    let st = r.status();
    if !st.is_success() {
        let v: Value = r.json().await.unwrap_or(Value::Null);
        bail!(
            "{st}: {} ({})",
            clean_line(v["error"]["message"].as_str().unwrap_or("request failed")),
            clean_line(v["error"]["code"].as_str().unwrap_or("-"))
        );
    }
    let (mut sse, mut out) = (SseParser::default(), std::io::stdout());
    let mut reply = Reply {
        text: String::new(),
        finish: None,
        completion_tokens: None,
    };
    let mut thinking = false;
    let mut s = r.bytes_stream();
    while let Some(chunk) = s.next().await {
        let chunk = chunk.map_err(|e| {
            if e.is_timeout() {
                anyhow!("the reply took longer than {} s", REPLY_TIMEOUT.as_secs())
            } else {
                anyhow::Error::new(e)
            }
        })?;
        for data in sse.push(&chunk)? {
            if data.trim() == "[DONE]" {
                println!();
                return Ok(reply);
            }
            let v: Value = serde_json::from_str(&data).context("malformed stream event")?;
            if let Some(e) = v.get("error") {
                bail!(
                    "{} ({})",
                    clean_line(e["message"].as_str().unwrap_or("error")),
                    clean_line(e["code"].as_str().unwrap_or("-"))
                );
            }
            if let Some(f) = v["choices"][0]["finish_reason"].as_str() {
                reply.finish = Some(f.to_string());
            }
            if let Some(n) = v["usage"]["completion_tokens"].as_i64() {
                reply.completion_tokens = Some(n);
            }
            let d = &v["choices"][0]["delta"];
            if let Some(t) = d["reasoning_content"].as_str() {
                eprint!("{}", clean(t));
                thinking = true;
            }
            if let Some(t) = d["content"].as_str() {
                if thinking && !t.is_empty() {
                    eprintln!(); // the answer starts on its own line after the thinking
                    thinking = false;
                }
                reply.text.push_str(t);
                write!(out, "{}", clean(t))?;
                out.flush()?;
            }
        }
    }
    bail!("the stream ended without [DONE]; the reply may be incomplete")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_sequences_are_removed() {
        assert_eq!(
            clean("a\x1b]0;pwn\x07b\x1b[2Jc\u{9b}d\r\ne\tf"),
            "a]0;pwnb[2Jcd\ne\tf"
        );
    }

    #[test]
    fn chat_only_talks_to_loopback_http() {
        for ok in [
            "http://127.0.0.1:8787/v1",
            "http://localhost:1/v1",
            "http://[::1]:8787/v1",
        ] {
            assert!(loopback_http(ok), "{ok}");
        }
        for bad in [
            "https://api.example.com/v1",
            "http://192.168.1.5:8787/v1",
            "http://127.0.0.1.evil.com/v1",
            "http://localhost@evil.com/v1",
            "http://127.0.0.1:80@evil.com/v1",
            "http://user:pw@127.0.0.1/v1",
            "ftp://127.0.0.1/",
            "not a url",
        ] {
            assert!(!loopback_http(bad), "{bad}");
        }
    }

    /// Every way of cutting the stream into reads gives the same events.
    fn events_split_everywhere(stream: &[u8]) -> Vec<String> {
        let whole = SseParser::default().push(stream).unwrap();
        for cut in 0..=stream.len() {
            let mut p = SseParser::default();
            let mut got = p.push(&stream[..cut]).unwrap();
            got.extend(p.push(&stream[cut..]).unwrap());
            assert_eq!(got, whole, "cut at {cut}");
        }
        let mut p = SseParser::default();
        let bytewise: Vec<String> = stream.iter().flat_map(|b| p.push(&[*b]).unwrap()).collect();
        assert_eq!(bytewise, whole, "byte by byte");
        whole
    }

    #[test]
    fn sse_events_are_split_by_lf_crlf_or_cr() {
        for sep in ["\n", "\r\n", "\r"] {
            let s =
                format!(": keepalive{sep}{sep}data: {{\"a\":1}}{sep}{sep}data:[DONE]{sep}{sep}");
            assert_eq!(
                events_split_everywhere(s.as_bytes()),
                ["{\"a\":1}", "[DONE]"],
                "{sep:?}"
            );
        }
        // Mixed endings, e.g. a CRLF event closed by the gateway's own "\n\n".
        assert_eq!(
            events_split_everywhere(b"data: x\r\n\ndata: y\n\r\n"),
            ["x", "y"]
        );
    }

    #[test]
    fn sse_multi_line_data_is_joined() {
        let s = b"data: {\"choices\":\ndata:  [1]}\r\nid: 7\r\nevent: message\r\n\r\n";
        assert_eq!(events_split_everywhere(s), ["{\"choices\":\n [1]}"]);
        let v: Value = serde_json::from_str(&events_split_everywhere(s)[0]).unwrap();
        assert_eq!(v["choices"][0], 1);
        // No data field, or an unfinished event at the end: nothing.
        assert!(events_split_everywhere(b"id: 1\n\nretry: 5\n\ndata: half").is_empty());
    }

    #[test]
    fn sse_oversized_event_is_an_error() {
        let mut p = SseParser::default();
        let big = vec![b'x'; MAX_SSE_EVENT + 1];
        assert!(p.push(&big).is_err());
    }

    #[test]
    fn chat_commands_need_an_exact_word() {
        use ChatInput::*;
        assert_eq!(chat_input("  "), Skip);
        assert_eq!(chat_input(" /exit "), Exit);
        assert_eq!(chat_input("/quit"), Exit);
        assert_eq!(chat_input("/new"), New);
        assert_eq!(chat_input("/system  Be brief. "), System("Be brief."));
        assert_eq!(chat_input("/system"), System(""));
        assert_eq!(chat_input("/systemfoo"), Help);
        assert_eq!(chat_input("/exit now"), Help);
        assert_eq!(chat_input("hello /exit"), Message);
    }

    /// Windows only, and only when the test's stdin is a console: echo goes off and the
    /// previous mode comes back. Not run on a Windows host yet.
    #[cfg(windows)]
    #[test]
    fn console_echo_is_turned_off_and_restored() {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
        };
        let mode = || {
            let mut m = 0;
            // SAFETY: reads the console mode of this process's stdin.
            (unsafe { GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut m) } != 0).then_some(m)
        };
        let Some(before) = mode() else {
            return; // stdin is not a console in this run
        };
        let guard = NoEcho::new().expect("console mode");
        assert_eq!(mode().unwrap() & ENABLE_ECHO_INPUT, 0);
        drop(guard);
        assert_eq!(mode(), Some(before));
    }

    #[test]
    fn listing_fields_stay_on_one_line() {
        assert_eq!(clean_line("a\tb\nc\x1b[2Jd"), "a b c[2Jd");
    }

    /// The prompt that shows while the typing is visible never claims otherwise, and says
    /// how to keep the secret off the screen.
    #[test]
    fn a_visible_prompt_says_so() {
        for (p, way_out) in [
            (&TUNNEL_TOKEN_PROMPT, "< token.txt"),
            (&CHAT_KEY_PROMPT, "SEMDOK_API_KEY"),
        ] {
            assert!(p.hidden.contains("typing is hidden"), "{}", p.hidden);
            assert!(!p.visible.contains("hidden"), "{}", p.visible);
            assert!(p.visible.contains("VISIBLE"), "{}", p.visible);
            assert!(p.visible.contains(way_out), "{}", p.visible);
        }
    }

    #[test]
    fn a_reply_cut_at_its_limit_is_marked_and_a_lowered_limit_named() {
        let plain =
            "[reply stopped at the token limit; use --max-tokens N, up to the server limit]";
        assert_eq!(length_notes(None, Some(1024)), [plain]);
        assert_eq!(length_notes(None, None), [plain]);
        assert_eq!(length_notes(Some(500), Some(500)), [plain]);
        assert_eq!(length_notes(Some(500), None), [plain]);
        let lowered = length_notes(Some(8000), Some(4096));
        assert_eq!(lowered[0], plain);
        assert!(
            lowered[1].contains("stopped after 4096 of the 8000 tokens asked for")
                && lowered[1].contains("--max-output-tokens")
                && lowered[1].contains("context window"),
            "{lowered:?}"
        );
    }

    /// After a failed `workspace get`: the file the path names is removed; a symlinked file is
    /// emptied and the link kept; nothing else is touched.
    #[cfg(unix)]
    #[test]
    fn a_failed_copy_removes_only_its_own_file() {
        let d = std::env::temp_dir().join(format!("semdok-edge-discard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let write = |p: &Path| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)
                .unwrap();
            f.write_all(b"half a co").unwrap();
            f
        };
        let plain = d.join("plain.txt");
        let f = write(&plain);
        assert!(discard_partial(&plain, f).contains("removed"));
        assert!(!plain.exists());

        let target = d.join("target.txt");
        let link = d.join("link.txt");
        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let f = write(&link);
        assert!(discard_partial(&link, f).contains("emptied"));
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"");

        // The path was replaced while the copy ran: the replacement stays.
        let moved = d.join("moved.txt");
        let f = write(&moved);
        std::fs::rename(&moved, d.join("elsewhere.txt")).unwrap();
        std::fs::write(&moved, "someone else's").unwrap();
        discard_partial(&moved, f);
        assert_eq!(std::fs::read(&moved).unwrap(), b"someone else's");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
