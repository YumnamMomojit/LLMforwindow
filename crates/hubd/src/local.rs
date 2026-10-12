//! Local-owner commands: workspaces, chat history settings, database backup, and a terminal
//! chat client for the local API. None of this is reachable through the gateway.
use crate::{usage_error, Args};
use anyhow::{anyhow, bail, Context, Result};
use fs4::fs_std::FileExt;
use futures_util::{FutureExt, StreamExt};
use hub_core::storage::{WorkspaceInfo, Workspaces};
use hub_core::{db, history};
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
        .with_context(|| format!("no workspace named or with id '{which}' (hubd workspace list)"))
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
                println!("no workspaces yet: hubd workspace create <name>");
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
            let n = match std::io::copy(&mut src, &mut out).and_then(|n| out.sync_all().map(|()| n))
            {
                Ok(n) => n,
                Err(e) => {
                    // Never leave a cut-off copy that looks complete.
                    drop(out);
                    let _ = std::fs::remove_file(&dest);
                    return Err(e).with_context(|| format!("writing {} failed", dest.display()));
                }
            };
            println!("wrote {} ({})", dest.display(), mb(n));
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
                    "no conversation with id '{}' (hubd history list)",
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

/// Terminal echo off while it lives; the saved mode is put back on drop.
struct NoEcho {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(windows)]
    saved: (windows_sys::Win32::Foundation::HANDLE, u32),
}

impl NoEcho {
    #[cfg(unix)]
    fn new() -> Option<Self> {
        // SAFETY: termios calls on stdin with a struct that tcgetattr fills in.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return None;
            }
            let saved = t;
            t.c_lflag &= !libc::ECHO;
            (libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) == 0).then_some(Self { saved })
        }
    }

    #[cfg(windows)]
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
        };
        // SAFETY: console mode calls on this process's own stdin handle.
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(h, &mut mode) == 0 {
                return None;
            }
            (SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) != 0).then_some(Self { saved: (h, mode) })
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restores the mode read in `new` on the same handle.
        #[cfg(unix)]
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(self.saved.0, self.saved.1);
        }
    }
}

/// One line from stdin for a key or token, without its line ending. At a terminal the prompt
/// goes to stderr and the typing is not shown, so the secret stays off the screen and out of
/// the scrollback; from a pipe the line is read as it comes, with no prompt. Ctrl-C cancels
/// either way (and puts the terminal back first).
pub(crate) async fn read_secret_line(prompt: &str, intr: &mut Interrupt) -> Result<String> {
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal();
    let echo = if tty {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        NoEcho::new()
    } else {
        None
    };
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
        bail!("hubd chat talks to the local API only (http://127.0.0.1:<port>/v1); for a remote endpoint use the examples/");
    }
    let base = o.url.trim_end_matches('/').to_string();
    let mut intr = Interrupt::new()?;
    let key = match std::env::var("HUB_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        Some(k) => k,
        None => {
            read_secret_line(
                "API key (lah_...; typing is hidden; set HUB_API_KEY to skip this): ",
                &mut intr,
            )
            .await?
        }
    };
    let key = key.trim().to_string();
    if !key.starts_with("lah_") {
        bail!("that is not a Local AI Hub key (they start with lah_); create one with `hubd key create <name>`");
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
        None => {
            let r = http
                .get(format!("{base}/models"))
                .bearer_auth(&key)
                .send()
                .await
                .with_context(|| format!("cannot reach {base}: is `hubd serve` running?"))?;
            let st = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            if !st.is_success() {
                bail!(
                    "{st}: {}",
                    clean_line(v["error"]["message"].as_str().unwrap_or("request failed"))
                );
            }
            v["data"][0]["id"]
                .as_str()
                .context("the server lists no model")?
                .to_string()
        }
    };
    // History is optional: without a usable database the chat still works, unsaved.
    let mut db_conn = if o.no_history {
        None
    } else {
        match db::open(&dir.join("hub.db")) {
            Ok(c) if history::enabled(&c) => Some(c),
            Ok(_) => None,
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
        if db_conn.is_some() { "saved locally (hubd history off to stop)" } else { "not saved" }
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
        let mut body = json!({"model": model, "messages": msgs, "stream": true});
        if let Some(m) = o.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let reply = tokio::select! {
            r = stream_reply(&http, &base, &key, &body) => r,
            _ = intr.recv() => Err(anyhow!("stopped")),
        };
        match reply {
            Ok(text) => {
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

/// Streams one reply to stdout and returns its text. Thinking (`reasoning_content`) is shown
/// on stderr and not kept in the conversation. Everything the server sends is shown inert.
async fn stream_reply(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    body: &Value,
) -> Result<String> {
    let r = http
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key)
        .json(body)
        .timeout(Duration::from_secs(3600))
        .send()
        .await
        .context("cannot reach the local API: is `hubd serve` running?")?;
    let st = r.status();
    if !st.is_success() {
        let v: Value = r.json().await.unwrap_or(Value::Null);
        bail!(
            "{st}: {} ({})",
            clean_line(v["error"]["message"].as_str().unwrap_or("request failed")),
            clean_line(v["error"]["code"].as_str().unwrap_or("-"))
        );
    }
    let (mut sse, mut text, mut out) = (SseParser::default(), String::new(), std::io::stdout());
    let mut thinking = false;
    let mut s = r.bytes_stream();
    while let Some(chunk) = s.next().await {
        for data in sse.push(&chunk?)? {
            if data.trim() == "[DONE]" {
                println!();
                return Ok(text);
            }
            let v: Value = serde_json::from_str(&data).context("malformed stream event")?;
            if let Some(e) = v.get("error") {
                bail!(
                    "{} ({})",
                    clean_line(e["message"].as_str().unwrap_or("error")),
                    clean_line(e["code"].as_str().unwrap_or("-"))
                );
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
                text.push_str(t);
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
}
