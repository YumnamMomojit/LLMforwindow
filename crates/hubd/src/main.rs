//! hubd - headless Local AI Hub service/CLI. The desktop shell calls the same hub-core APIs.
mod local;
use anyhow::{anyhow, bail, Context, Result};
use hub_core::engine::{ColibriAdapter, ColibriConfig, EngineState, InferenceAdapter};
use hub_core::gateway::{Gateway, GatewayConfig};
use hub_core::secrets::{self, FileSecrets, SecretStore};
use hub_core::tunnel::{CloudflaredTunnel, PublicSharingConsent, TunnelState};
use hub_core::{db, history, keys, models};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const USAGE: &str = "hubd - Local AI Hub

USAGE: hubd [--data-dir DIR] <command>

  init                                   create data folders and database
  model validate <dir> [--colibri-dir C] [--python P]
  model add <dir> --id ID [--colibri-dir C] [--python P]  (references in place; no copy)
  model list | model remove <id>        (catalog entry only; files are never deleted)
  key create <name> [--expires-days N]  (N = 1..3650; secret shown ONCE)
  key list | key revoke <id> | key rotate <id> [--grace-hours N]   (N = 0..720, default 24)
  secret set tunnel_token               (reads one line from stdin; typing at a terminal is hidden)
  serve --model ID [--colibri-dir C] [--python P] [--port N] [--max-active N]
        [--startup-timeout SECS] [--share --cloudflared EXE --hostname H --i-authorize-public-sharing]
  purge                                  apply history/log retention
  chat [--model ID] [--system TEXT] [--max-tokens N] [--no-history] [--url http://127.0.0.1:8787/v1]
                                         terminal chat with the running local API (key from $HUB_API_KEY or a
                                         hidden prompt); Ctrl-C stops a reply, at the prompt it ends the chat
  history status | on | off | retention <days|none> | list | show <id> | delete <id> --yes
  workspace list | create <name> [--quota-mb N] | files <ws>
  workspace put <ws> <file> [--as REL] [--overwrite] | get <ws> <rel> <dest> [--overwrite]
  workspace rm <ws> <rel> --yes | delete <ws> --yes | export <ws> <dest.zip> [--overwrite]
  backup <dest.db>                       consistent database snapshot (no keys, no request log)
  help | --help | -h                     show this text and do nothing else

--colibri-dir defaults to $HUB_COLIBRI_DIR; --python to $HUB_PYTHON, else python3 (python.exe) on PATH.
Local only by default. Sharing starts only with --share AND --i-authorize-public-sharing.";

/// Longest key lifetime `key create --expires-days` accepts (10 years).
const MAX_KEY_DAYS: i64 = 3650;
/// Longest overlap `key rotate --grace-hours` accepts (30 days).
const MAX_GRACE_HOURS: i64 = 720;

/// `println!`/`eprintln!` that never panic. After SIGHUP the terminal is gone, and a failed
/// write must not abort the engine teardown that follows it.
macro_rules! say {
    ($($t:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stdout(), $($t)*); }};
}
macro_rules! say_err {
    ($($t:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stderr(), $($t)*); }};
}

fn usage_error(msg: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{msg}\n\n{USAGE}")
}

/// The arguments not consumed yet. A command takes the flags and words it knows, then calls
/// `finish`, so unknown flags, extra words and repeated flags are errors instead of being
/// silently ignored.
struct Args(Vec<String>);

impl Args {
    /// `NAME VALUE` anywhere in the list. A flag without a value (last argument, an empty
    /// string, or followed by another `--flag`) is an error, never "flag absent".
    fn value(&mut self, name: &str) -> Result<Option<String>> {
        let Some(i) = self.0.iter().position(|a| a == name) else {
            return Ok(None);
        };
        match self.0.get(i + 1) {
            Some(v) if !v.is_empty() && !v.starts_with("--") => {
                let v = v.clone();
                self.0.drain(i..i + 2);
                Ok(Some(v))
            }
            _ => Err(usage_error(format!("{name} needs a value"))),
        }
    }

    fn switch(&mut self, name: &str) -> bool {
        match self.0.iter().position(|a| a == name) {
            Some(i) => {
                self.0.remove(i);
                true
            }
            None => false,
        }
    }

    /// The next word (command, subcommand or positional argument). Call it for positionals
    /// only after every flag of the command has been taken.
    fn word(&mut self, what: &str) -> Result<String> {
        match self.0.first() {
            None => Err(usage_error(format!("missing {what}"))),
            Some(a) if a.is_empty() || a.starts_with("--") => {
                Err(usage_error(format!("expected {what}, got '{a}'")))
            }
            Some(_) => Ok(self.0.remove(0)),
        }
    }

    fn finish(self) -> Result<()> {
        match self.0.first() {
            None => Ok(()),
            Some(a) => Err(usage_error(format!("unexpected argument '{a}'"))),
        }
    }
}

/// A whole number in `min..=max`, or a usage error naming the flag.
fn bounded(name: &str, v: &str, min: i64, max: i64) -> Result<i64> {
    match v.parse::<i64>() {
        Ok(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(usage_error(format!(
            "{name} must be a whole number from {min} to {max}, got '{v}'"
        ))),
    }
}

/// A path from `--flag`, else from a non-empty environment variable.
fn path_arg(args: &mut Args, flag: &str, env: &str) -> Result<Option<PathBuf>> {
    if let Some(v) = args.value(flag)? {
        return Ok(Some(PathBuf::from(v)));
    }
    Ok(std::env::var_os(env)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from))
}

fn colibri_arg(args: &mut Args) -> Result<PathBuf> {
    path_arg(args, "--colibri-dir", "HUB_COLIBRI_DIR")?
        .ok_or_else(|| usage_error("--colibri-dir C is required (or set HUB_COLIBRI_DIR)"))
}

fn python_arg(args: &mut Args) -> Result<PathBuf> {
    Ok(resolve_exe(
        path_arg(args, "--python", "HUB_PYTHON")?.unwrap_or_else(default_python),
    ))
}

fn data_dir(args: &mut Args) -> Result<PathBuf> {
    if let Some(v) = args.value("--data-dir")? {
        return Ok(PathBuf::from(v));
    }
    if let Some(v) = std::env::var_os("HUB_DATA_DIR") {
        if v.is_empty() {
            bail!("HUB_DATA_DIR is set but empty");
        }
        return Ok(PathBuf::from(v));
    }
    Ok(if cfg!(windows) {
        PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()))
            .join("LocalAIHub")
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".local/share/local-ai-hub")
    })
}

fn default_python() -> PathBuf {
    PathBuf::from(if cfg!(windows) {
        "python.exe"
    } else {
        "python3"
    })
}

fn resolve_exe(p: PathBuf) -> PathBuf {
    if p.components().count() > 1 || p.is_absolute() {
        return p;
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(&p))
                .find(|c| c.is_file())
        })
        .unwrap_or(p)
}

fn find_helper() -> Result<PathBuf> {
    std::env::current_exe()?
        .ancestors()
        .find_map(|a| {
            let p = a.join("scripts/validate_model.py");
            p.is_file().then_some(p)
        })
        .or_else(|| {
            let p = PathBuf::from("scripts/validate_model.py");
            p.is_file().then_some(p)
        })
        .context("scripts/validate_model.py not found")
}

/// Windows `canonicalize` returns verbatim paths (`\\?\D:\models\m`). The C engine appends
/// `/config.json` to the model path, and Win32 does not translate `/` inside a verbatim path,
/// so the engine could never open its files. Store and pass the ordinary form instead; like
/// the `dunce` crate, strip the prefix only when the plain path names the same file.
fn engine_path(p: PathBuf) -> Result<PathBuf> {
    match p.to_str() {
        Some(s) if s.starts_with(r"\\?\") => {
            plain_windows_path(s).map(PathBuf::from).with_context(|| {
                format!(
                    "{s} has no ordinary Windows form (260+ characters, or a reserved or \
                     invalid name); move the model to a shorter, plainly named folder"
                )
            })
        }
        _ => Ok(p),
    }
}

/// `\\?\C:\a\b` -> `C:\a\b`, `\\?\UNC\srv\share\a` -> `\\srv\share\a`. `None` for other verbatim
/// kinds and whenever Win32 would read the plain form differently: MAX_PATH or longer, `.`/`..`,
/// a trailing dot or space, a reserved device name, or a character Win32 rejects.
fn plain_windows_path(s: &str) -> Option<String> {
    let (plain, names) = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        (format!(r"\\{rest}"), rest)
    } else {
        let rest = s.strip_prefix(r"\\?\")?;
        let b = rest.as_bytes();
        if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
            return None;
        }
        (rest.to_string(), &rest[3..])
    };
    const RESERVED: [&str; 30] = [
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
        "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT1", "LPT2", "LPT3", "LPT4",
        "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
    ];
    let plain_name = |n: &str| {
        let stem = n.split('.').next().unwrap_or("").trim_end_matches(' ');
        !n.is_empty()
            && n != "."
            && n != ".."
            && !n.ends_with(['.', ' '])
            && !n.chars().any(|c| c < ' ' || r#"<>:"/|?*"#.contains(c))
            && !RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
    };
    let names = names.strip_suffix('\\').unwrap_or(names);
    let ok = plain.encode_utf16().count() < 260
        && (names.is_empty() || names.split('\\').all(plain_name));
    ok.then_some(plain)
}

fn fmt_time(t: Option<i64>) -> String {
    t.map_or("-".into(), |v| v.to_string())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // The cause chain on one line; never a backtrace, whatever RUST_BACKTRACE says.
            say_err!("Error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // Help wins wherever it appears and has no side effects (`purge --help` must not purge).
    if raw.is_empty() || raw[0] == "help" || raw.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let mut args = Args(raw);
    let dir = data_dir(&mut args)?;
    let cmd = args.word("command")?;
    let db_path = dir.join("hub.db");
    // Every arm parses all of its arguments before touching the data directory.
    match cmd.as_str() {
        "help" => println!("{USAGE}"),
        "init" => {
            args.finish()?;
            for d in ["", "workspaces", "exports", "cache", "logs", "secrets"] {
                std::fs::create_dir_all(dir.join(d))?;
            }
            let c = db::open(&db_path)?;
            let store = FileSecrets::new(&dir.join("secrets"))?;
            secrets::internal_key(&store)?;
            println!(
                "initialised {} (schema v{}, secrets: {})",
                dir.display(),
                db::schema_version(&c),
                store.protection()
            );
        }
        "model" => {
            let sub = args.word("model subcommand (list, remove, validate, add)")?;
            match sub.as_str() {
                "list" => {
                    args.finish()?;
                    let c = db::open(&db_path)?;
                    for m in models::list(&c)? {
                        println!(
                            "{}\t{}\t{}\t{} bytes",
                            m.id,
                            m.model_type,
                            m.path.display(),
                            m.weights_bytes
                        )
                    }
                }
                "remove" => {
                    let id = args.word("model id")?;
                    args.finish()?;
                    let c = db::open(&db_path)?;
                    println!(
                        "{}",
                        if models::unregister(&c, &id)? {
                            "removed from catalog (files untouched)"
                        } else {
                            "not found"
                        }
                    )
                }
                "validate" | "add" => {
                    let colibri = colibri_arg(&mut args)?;
                    let python = python_arg(&mut args)?;
                    let id = if sub == "add" {
                        let id = args
                            .value("--id")?
                            .ok_or_else(|| usage_error("--id ID is required"))?;
                        if !models::valid_model_id(&id) {
                            bail!("model id must be 1-64 chars of [A-Za-z0-9._-]");
                        }
                        Some(id)
                    } else {
                        None
                    };
                    let mdir = PathBuf::from(args.word("model directory")?);
                    args.finish()?;
                    let helper = find_helper()?;
                    match models::validate(&mdir, &python, &colibri, &helper).await {
                        Ok(info) => {
                            println!(
                                "supported: {} ({}), weights {} bytes",
                                info.display_name, info.family, info.weights_bytes
                            );
                            let pf = models::preflight(info.weights_bytes, &mdir, 0);
                            println!(
                                "{} - RAM {:?} bytes, free disk {:?} bytes",
                                pf.label, pf.ram_total_bytes, pf.free_disk_bytes
                            );
                            if let Some(id) = id {
                                let path = engine_path(mdir.canonicalize()?)?;
                                let c = db::open(&db_path)?;
                                models::register(&c, &id, &path, &info)?;
                                println!("added (referenced in place): {}", path.display());
                            }
                        }
                        Err(issues) => {
                            for i in &issues {
                                eprintln!("- {i}");
                            }
                            bail!("model not usable");
                        }
                    }
                }
                _ => return Err(usage_error(format!("unknown model subcommand '{sub}'"))),
            }
        }
        "key" => {
            let sub = args.word("key subcommand (create, list, revoke, rotate)")?;
            match sub.as_str() {
                "create" => {
                    let days = args
                        .value("--expires-days")?
                        .map(|v| bounded("--expires-days", &v, 1, MAX_KEY_DAYS))
                        .transpose()?;
                    let name = args.word("key name")?;
                    args.finish()?;
                    let exp = days
                        .map(|d| {
                            d.checked_mul(86_400)
                                .and_then(|s| hub_core_now().checked_add(s))
                                .context("expiry out of range")
                        })
                        .transpose()?;
                    let c = db::open(&db_path)?;
                    let (rec, tok) = keys::create(&c, &name, exp)?;
                    println!(
                        "id:      {}\nname:    {}\nscopes:  {}\nexpires: {}\nsecret (shown once, store it now):\n{tok}",
                        rec.id,
                        rec.name,
                        rec.scopes.join(","),
                        days.map_or("never".into(), |d| format!("in {d} days ({})", fmt_time(exp)))
                    );
                }
                "list" => {
                    args.finish()?;
                    let c = db::open(&db_path)?;
                    for k in keys::list(&c)? {
                        println!(
                            "{}\t{}\tscopes={}\tcreated={}\texpires={}\trevoked={}\tlast_used={}",
                            k.id,
                            k.name,
                            k.scopes.join(","),
                            k.created_at,
                            fmt_time(k.expires_at),
                            fmt_time(k.revoked_at),
                            fmt_time(k.last_used_at)
                        )
                    }
                }
                "revoke" => {
                    let id = args.word("key id")?;
                    args.finish()?;
                    let c = db::open(&db_path)?;
                    println!(
                        "{}",
                        if keys::revoke(&c, &id)? {
                            "revoked"
                        } else {
                            "no active key with that id"
                        }
                    )
                }
                "rotate" => {
                    let hrs = args
                        .value("--grace-hours")?
                        .map(|v| bounded("--grace-hours", &v, 0, MAX_GRACE_HOURS))
                        .transpose()?
                        .unwrap_or(24);
                    let id = args.word("key id")?;
                    args.finish()?;
                    let grace = hrs.checked_mul(3600).context("grace out of range")?;
                    let c = db::open(&db_path)?;
                    let (rec, tok) = keys::rotate(&c, &id, grace)?;
                    let old = keys::get(&c, &id)?.context("no such key")?;
                    // The old key ends at its grace end or its own expiry, whichever is first.
                    let old_end = old
                        .revoked_at
                        .map(|r| old.expires_at.map_or(r, |e| e.min(r)));
                    println!(
                        "new id: {}\nsecret (shown once):\n{tok}\nnew key expires: {}\nold key {id} stays valid until {} (unix time; grace requested: {hrs} h)",
                        rec.id,
                        fmt_time(rec.expires_at),
                        fmt_time(old_end)
                    );
                }
                _ => return Err(usage_error(format!("unknown key subcommand '{sub}'"))),
            }
        }
        "secret" => {
            let sub = args.word("secret subcommand (set)")?;
            let name = args.word("secret name (tunnel_token)")?;
            args.finish()?;
            if sub != "set" || name != "tunnel_token" {
                return Err(usage_error("only `secret set tunnel_token` is supported"));
            }
            let mut intr = local::Interrupt::new()?;
            let v = local::read_secret_line("tunnel token (typing is hidden): ", &mut intr).await?;
            let v = v.trim();
            if v.is_empty() {
                bail!("no token on stdin; nothing stored");
            }
            FileSecrets::new(&dir.join("secrets"))?.set("tunnel_token", v.as_bytes())?;
            println!("stored");
        }
        "purge" => {
            args.finish()?;
            let c = db::open(&db_path)?;
            println!(
                "conversations removed: {}, log rows removed: {}",
                history::purge_expired(&c)?,
                history::purge_request_log(&c, 30)?
            );
        }
        "workspace" => local::workspace(&dir, args)?,
        "history" => local::history_cmd(&dir, args)?,
        "backup" => local::backup(&dir, args)?,
        "chat" => local::chat(&dir, args).await?,
        "serve" => {
            let opts = ServeOpts::parse(args)?;
            serve(&dir, &db_path, opts).await?
        }
        _ => return Err(usage_error(format!("unknown command '{cmd}'"))),
    }
    Ok(())
}

fn hub_core_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct ShareOpts {
    cloudflared: PathBuf,
    hostname: String,
}

struct ServeOpts {
    model_id: String,
    colibri: PathBuf,
    python: PathBuf,
    port: u16,
    max_active: usize,
    startup_timeout: Duration,
    share: Option<ShareOpts>,
}

impl ServeOpts {
    fn parse(mut args: Args) -> Result<Self> {
        let model_id = args
            .value("--model")?
            .ok_or_else(|| usage_error("--model ID is required"))?;
        let colibri = colibri_arg(&mut args)?;
        let python = python_arg(&mut args)?;
        let port = match args.value("--port")? {
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| usage_error(format!("--port must be 0-65535, got '{p}'")))?,
            None => 8787,
        };
        let max_active = match args.value("--max-active")? {
            Some(v) => bounded("--max-active", &v, 1, 16)? as usize,
            None => 1,
        };
        let startup_timeout = match args.value("--startup-timeout")? {
            Some(v) => bounded("--startup-timeout", &v, 1, 86_400)? as u64,
            None => 900,
        };
        let share = args.switch("--share");
        let consent = args.switch("--i-authorize-public-sharing");
        let cloudflared = args.value("--cloudflared")?;
        let hostname = args.value("--hostname")?;
        args.finish()?;
        let share = match (share, consent, cloudflared, hostname) {
            (false, false, None, None) => None,
            (true, true, Some(exe), Some(hostname)) => Some(ShareOpts {
                cloudflared: resolve_exe(PathBuf::from(exe)),
                hostname,
            }),
            (true, ..) => bail!("--share needs --cloudflared EXE, --hostname H and --i-authorize-public-sharing (explicit consent to publish)"),
            (false, ..) => bail!("--cloudflared, --hostname and --i-authorize-public-sharing only apply together with --share"),
        };
        Ok(Self {
            model_id,
            colibri,
            python,
            port,
            max_active,
            startup_timeout: Duration::from_secs(startup_timeout),
            share,
        })
    }
}

/// Stop requests hubd handles itself so that the engine tree is always torn down: Ctrl-C,
/// SIGTERM and SIGHUP on Unix, where the engine runs in its own process group and never sees
/// the terminal's signals; Ctrl-C, Ctrl-Break and console close on Windows, where the Job
/// Object also ends the tree if Windows does not wait for the orderly stop. Installed before
/// the engine starts, so a stop request during a long model load still stops the engine.
/// A SIGHUP that was already ignored when hubd started (`nohup hubd serve &`, a supervisor)
/// stays ignored: listening for it would replace that SIG_IGN and end serving at logout.
struct Shutdown {
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sighup: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    ctrl_close: tokio::signal::windows::CtrlClose,
}

impl Shutdown {
    #[cfg(unix)]
    fn install() -> Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        let sighup = if sighup_ignored() {
            say!("note: SIGHUP is ignored (nohup); closing the terminal does not stop serving");
            None
        } else {
            Some(signal(SignalKind::hangup())?)
        };
        Ok(Self {
            sigint: signal(SignalKind::interrupt())?,
            sigterm: signal(SignalKind::terminate())?,
            sighup,
        })
    }

    #[cfg(unix)]
    async fn recv(&mut self) -> &'static str {
        let Self {
            sigint,
            sigterm,
            sighup,
        } = self;
        let hup = async {
            match sighup {
                Some(s) => s.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = sigint.recv() => "SIGINT",
            _ = sigterm.recv() => "SIGTERM",
            _ = hup => "SIGHUP",
        }
    }

    #[cfg(windows)]
    fn install() -> Result<Self> {
        use tokio::signal::windows;
        Ok(Self {
            ctrl_c: windows::ctrl_c()?,
            ctrl_break: windows::ctrl_break()?,
            ctrl_close: windows::ctrl_close()?,
        })
    }

    #[cfg(windows)]
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.ctrl_c.recv() => "Ctrl-C",
            _ = self.ctrl_break.recv() => "Ctrl-Break",
            _ = self.ctrl_close.recv() => "console close",
        }
    }
}

/// True if this process started with SIGHUP set to SIG_IGN (what `nohup` does).
#[cfg(unix)]
fn sighup_ignored() -> bool {
    // SAFETY: a null new action only reads the current disposition into `old`.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) == 0
            && old.sa_sigaction == libc::SIG_IGN
    }
}

async fn serve(dir: &Path, db_path: &Path, o: ServeOpts) -> Result<()> {
    let c = db::open(db_path)?;
    let rec = models::get(&c, &o.model_id)?.with_context(|| {
        format!(
            "model '{}' is not in the catalog (hubd model add)",
            o.model_id
        )
    })?;
    // Records added before verbatim paths were stripped still work.
    let model_dir = engine_path(rec.path)?;
    let t = hub_core_now();
    if keys::list(&c)?
        .iter()
        .all(|k| k.revoked_at.is_some_and(|r| r <= t) || k.expires_at.is_some_and(|e| e <= t))
    {
        say_err!("note: no active API keys; create one with `hubd key create <name>`");
    }
    let store = FileSecrets::new(&dir.join("secrets"))?;
    let internal = secrets::internal_key(&store)?;
    // Everything that can fail without the engine is checked before the model load, which
    // can take many minutes: a taken port, a missing token or a mistyped cloudflared path
    // must not be reported only after it.
    let listen = SocketAddr::from(([127, 0, 0, 1], o.port));
    std::net::TcpListener::bind(listen).with_context(|| {
        format!(
            "port {} on 127.0.0.1 is not available; choose another with --port",
            o.port
        )
    })?;
    let share = o.share.map(|s| prepare_share(&store, s)).transpose()?;
    let mut shutdown = Shutdown::install().context("could not install stop handlers")?;
    let mut engine = ColibriAdapter::new(ColibriConfig {
        python: o.python,
        colibri_dir: o.colibri,
        engine_exe: None,
        model_dir: model_dir.clone(),
        model_id: o.model_id.clone(),
        port: 0,
        internal_key: internal.clone(),
        startup_timeout: o.startup_timeout,
        max_tokens: 1024,
    });
    say!("starting engine for {} ...", model_dir.display());
    let started = tokio::select! {
        r = engine.start() => Ok(r),
        s = shutdown.recv() => Err(s),
    };
    // No `?` from here to `engine.stop()`: every outcome must reach it.
    let result = match started {
        Ok(Ok(())) => match engine.upstream_url() {
            Some(upstream) => {
                let gw = GatewayConfig {
                    listen,
                    upstream,
                    model_id: o.model_id,
                    db_path: db_path.to_path_buf(),
                    max_active: o.max_active,
                    ..Default::default()
                };
                serve_ready(&engine, &mut shutdown, gw, internal, share).await
            }
            None => Err(anyhow!("the engine reported ready without an address")),
        },
        Ok(Err(e)) => Err(anyhow!("engine failed: {e}")),
        Err(sig) => {
            say!("{sig} received during engine startup; stopping ...");
            Ok(())
        }
    };
    // Every path ends here, so the engine tree is torn down whether serving ended by a stop
    // request, an engine failure, or an error (gateway bind, tunnel start) after the start.
    engine.stop().await;
    say!("stopped");
    result
}

/// Reads and checks what `--share` needs, before the engine starts.
fn prepare_share(store: &FileSecrets, s: ShareOpts) -> Result<(CloudflaredTunnel, String)> {
    let token = store
        .get("tunnel_token")?
        .context("no tunnel token stored (hubd secret set tunnel_token)")?;
    let token = String::from_utf8(token)
        .context("the stored tunnel token is not text; store it again")?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("the stored tunnel token is empty; store it again (hubd secret set tunnel_token)");
    }
    if !s.cloudflared.is_file() {
        bail!("cloudflared not found at {}", s.cloudflared.display());
    }
    Ok((CloudflaredTunnel::new(s.cloudflared, s.hostname), token))
}

/// Gateway, optional tunnel, then the watch loop. Stops the tunnel and the gateway on every
/// path; the caller stops the engine.
async fn serve_ready(
    engine: &ColibriAdapter,
    shutdown: &mut Shutdown,
    cfg: GatewayConfig,
    internal: String,
    share: Option<(CloudflaredTunnel, String)>,
) -> Result<()> {
    let model_id = cfg.model_id.clone();
    let gw = Gateway::start(cfg, internal)
        .await
        .context("could not start the local API")?;
    say!(
        "local API ready:  http://{}/v1   model id: {model_id}   (auth: Bearer <key>)",
        gw.addr
    );
    let mut tunnel = None;
    let started = match share {
        None => {
            say!("sharing: OFF (local only, nothing is published)");
            Ok(())
        }
        Some((mut t, token)) => match t
            .start(&token, &PublicSharingConsent::granted_by_user())
            .await
        {
            Ok(()) => {
                say!(
                    "sharing: CONNECTING https://{}/v1 (not public until cloudflared registers a connection)",
                    t.public_hostname
                );
                tunnel = Some(t);
                Ok(())
            }
            Err(e) => Err(anyhow!("sharing could not start: {e}")),
        },
    };
    let result = match started {
        Ok(()) => {
            say!("Ctrl-C to stop. Serving ends when this process ends.");
            watch(engine, &mut tunnel, shutdown).await
        }
        Err(e) => Err(e),
    };
    say!("stopping ...");
    if let Some(mut t) = tunnel {
        t.stop().await;
    }
    gw.shutdown().await;
    result
}

/// Runs until a stop request (Ok) or an engine failure (Err, so the exit code is nonzero),
/// printing every change of the tunnel state.
async fn watch(
    engine: &ColibriAdapter,
    tunnel: &mut Option<CloudflaredTunnel>,
    shutdown: &mut Shutdown,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut shown = TunnelState::Connecting;
    let since = Instant::now();
    let mut warned = false;
    loop {
        tokio::select! {
            s = shutdown.recv() => {
                say!("{s} received");
                return Ok(());
            }
            _ = tick.tick() => {}
        }
        if let EngineState::Failed(m) = engine.state() {
            bail!("engine failed: {m}");
        }
        let Some(t) = tunnel.as_mut() else { continue };
        let now = t.state();
        if now != shown {
            match &now {
                TunnelState::Connected => say!(
                    "sharing: CONNECTED https://{}/v1 (verify from another network with scripts/verify_remote.py)",
                    t.public_hostname
                ),
                TunnelState::Connecting => say!("sharing: CONNECTING (reconnecting)"),
                TunnelState::Off => say!("sharing: OFF (tunnel stopped)"),
                TunnelState::Failed(m) => say_err!(
                    "sharing: FAILED ({m}); nothing is published, the local API keeps running"
                ),
            }
            shown = now.clone();
        }
        if matches!(now, TunnelState::Failed(_)) {
            t.stop().await;
            *tunnel = None;
        } else if now == TunnelState::Connecting && !warned && since.elapsed().as_secs() >= 60 {
            warned = true;
            say_err!("sharing: still not connected after 60 s; check the tunnel token, the hostname and cloudflared");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_paths_are_made_plain() {
        let p = plain_windows_path;
        assert_eq!(p(r"\\?\D:\models\m").as_deref(), Some(r"D:\models\m"));
        assert_eq!(p(r"\\?\D:\").as_deref(), Some(r"D:\"));
        assert_eq!(
            p(r"\\?\UNC\srv\share\models\m").as_deref(),
            Some(r"\\srv\share\models\m")
        );
        assert_eq!(
            p(r"\\?\C:\Users\Zoë\my models\qwen3.5").as_deref(),
            Some(r"C:\Users\Zoë\my models\qwen3.5")
        );
    }

    #[test]
    fn verbatim_paths_that_win32_would_reinterpret_are_kept_out() {
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        for bad in [
            r"\\?\C:",                          // drive-relative once stripped
            r"\\?\C:models",                    // drive-relative once stripped
            r"\\?\C:\models\con",               // device name
            r"\\?\C:\models\NUL.txt",           // device name with an extension
            r"\\?\C:\models\com1 .json",        // trailing spaces before the dot
            r"\\?\C:\models\m.",                // trailing dot is dropped by Win32
            r"\\?\C:\models\m ",                // trailing space is dropped by Win32
            r"\\?\C:\models\..\m",              // literal `..` in a verbatim path
            r"\\?\C:\models\a/b",               // `/` is a separator only outside \\?\
            r"\\?\C:\models\\m",                // empty component
            r"\\?\Volume{1b3b1146}\models\m",   // volume GUID: no drive-letter form
            r"\\?\GLOBALROOT\Device\Harddisk0", // device namespace
            &long,                              // MAX_PATH
        ] {
            assert_eq!(plain_windows_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn engine_path_strips_only_verbatim_prefixes() {
        assert_eq!(
            engine_path(PathBuf::from("/srv/models/m")).unwrap(),
            PathBuf::from("/srv/models/m")
        );
        assert_eq!(
            engine_path(PathBuf::from(r"\\?\D:\models\m")).unwrap(),
            PathBuf::from(r"D:\models\m")
        );
        assert!(engine_path(PathBuf::from(r"\\?\D:\models\aux")).is_err());
    }

    /// Windows only: the real `canonicalize` result is stored without `\\?\` and still names
    /// the same folder. Not run on a Windows host yet.
    #[cfg(windows)]
    #[test]
    fn canonical_windows_model_path_is_stored_plain() {
        let d = std::env::temp_dir().join(format!("hubd-verbatim-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let canon = d.canonicalize().unwrap();
        assert!(canon.to_str().unwrap().starts_with(r"\\?\"));
        let plain = engine_path(canon.clone()).unwrap();
        assert!(!plain.to_str().unwrap().starts_with(r"\\?\"));
        assert_eq!(plain.canonicalize().unwrap(), canon);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn args_reject_missing_values_and_leftovers() {
        let mut a = Args(vec!["--expires-days".into()]);
        assert!(a.value("--expires-days").is_err());
        let mut a = Args(vec!["--id".into(), "--python".into(), "p".into()]);
        assert!(a.value("--id").is_err());
        let mut a = Args(vec![
            "--port".into(),
            "1".into(),
            "--port".into(),
            "2".into(),
        ]);
        assert_eq!(a.value("--port").unwrap().as_deref(), Some("1"));
        assert!(a.finish().is_err());
        let mut a = Args(vec![]);
        assert!(a.word("key id").is_err());
    }

    #[test]
    fn bounded_numbers() {
        assert_eq!(
            bounded("--expires-days", "90", 1, MAX_KEY_DAYS).unwrap(),
            90
        );
        for bad in ["0", "-5", "3651", "9999999999999999999", "1e3", " 9", ""] {
            assert!(
                bounded("--expires-days", bad, 1, MAX_KEY_DAYS).is_err(),
                "{bad}"
            );
        }
    }
}
