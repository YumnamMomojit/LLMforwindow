//! Local commands through the real binary: `workspace get --overwrite` into a FIFO or through a
//! symlink to a device writes and never removes the destination; and a hidden key or token
//! prompt ended by SIGTERM, SIGQUIT or SIGHUP leaves the terminal echoing, ends hubd by that
//! signal, and gives those signals back their previous action once the prompt is closed
//! (driven through a pty by python3; skipped without it). Linux only (/proc for the signal
//! masks).
#![cfg(target_os = "linux")]
use std::io::Read;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cli3-local-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
    fn data(&self) -> PathBuf {
        self.0.join("data")
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_hubd"))
            .arg("--data-dir")
            .arg(self.data())
            .args(args)
            .env("RUST_BACKTRACE", "0")
            .env_remove("HUB_DATA_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        o
    }
    /// A workspace `A` holding `notes.txt`.
    fn with_notes(tag: &str) -> Self {
        let s = Scratch::new(tag);
        s.ok(&["init"]);
        s.ok(&["workspace", "create", "A"]);
        let src = s.0.join("notes.txt");
        std::fs::write(&src, "hello notes").unwrap();
        s.ok(&["workspace", "put", "A", src.to_str().unwrap()]);
        s
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn workspace_get_into_a_fifo_writes_it_and_keeps_it() {
    let s = Scratch::with_notes("get-fifo");
    let fifo = s.0.join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: plain mkfifo(3) on a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let reader = {
        let fifo = fifo.clone();
        std::thread::spawn(move || {
            let mut got = String::new();
            std::fs::File::open(&fifo)
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            got
        })
    };
    let o = s.ok(&[
        "workspace",
        "get",
        "A",
        "notes.txt",
        fifo.to_str().unwrap(),
        "--overwrite",
    ]);
    assert_eq!(reader.join().unwrap(), "hello notes");
    assert!(
        std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo(),
        "the FIFO was removed or replaced"
    );
    // The summary stays out of the stream (stdout may be the destination itself).
    assert!(String::from_utf8_lossy(&o.stdout).is_empty());
    assert!(String::from_utf8_lossy(&o.stderr).contains("wrote"));
}

#[test]
fn workspace_get_through_a_symlink_to_a_device_keeps_the_link() {
    let s = Scratch::with_notes("get-devnull");
    let link = s.0.join("nul-link");
    std::os::unix::fs::symlink("/dev/null", &link).unwrap();
    s.ok(&[
        "workspace",
        "get",
        "A",
        "notes.txt",
        link.to_str().unwrap(),
        "--overwrite",
    ]);
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    // A regular file is still written, synced and reported on stdout.
    let dest = s.0.join("copy.txt");
    let o = s.ok(&["workspace", "get", "A", "notes.txt", dest.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello notes");
    assert!(String::from_utf8_lossy(&o.stdout).contains("wrote"));
}

fn python_available() -> bool {
    Command::new("python3")
        .args(["-c", "import pty, termios, resource"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs hubd as a session leader on a fresh pty, with SIGTERM, SIGQUIT and SIGINT at their
/// default action, SIGHUP default or ignored (`nohup`), and no core files. Waits for `wait`,
/// records echo and the signal masks; optionally types `type` and waits for `then_wait`,
/// recording the masks again; sends `signal`; reports the exit and whether echo is on.
const PTY_DRIVER: &str = r#"
import fcntl, json, os, resource, select, signal, subprocess, sys, termios, time
cfg = json.loads(sys.argv[1])
master, slave = os.openpty()
def setup():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    for s in (signal.SIGTERM, signal.SIGQUIT, signal.SIGINT):
        signal.signal(s, signal.SIG_DFL)
    signal.signal(signal.SIGHUP, signal.SIG_IGN if cfg.get("nohup") else signal.SIG_DFL)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
p = subprocess.Popen(sys.argv[2:], stdin=slave, stdout=slave, stderr=slave, preexec_fn=setup)
out = b""
def pump(until, limit):
    global out
    end = time.time() + limit
    while time.time() < end and not until():
        r, _, _ = select.select([master], [], [], 0.05)
        if r:
            try:
                out += os.read(master, 4096)
            except OSError:
                break
def masks():
    st = open(f"/proc/{p.pid}/status").read()
    def bits(name):
        m = int(next(l.split()[1] for l in st.splitlines() if l.startswith(name + ":")), 16)
        return [s for s in range(1, 65) if m >> (s - 1) & 1]
    return {"caught": bits("SigCgt"), "ignored": bits("SigIgn")}
r = {}
pump(lambda: cfg["wait"].encode() in out, 20)
time.sleep(0.3)
r["echo_at_prompt"] = bool(termios.tcgetattr(slave)[3] & termios.ECHO)
r["at_prompt"] = masks()
if "type" in cfg:
    os.write(master, cfg["type"].encode())
    pump(lambda: cfg["then_wait"].encode() in out, 20)
    time.sleep(0.3)
    r["after_prompt"] = masks()
os.kill(p.pid, cfg["signal"])
pump(lambda: p.poll() is not None, 20)
pump(lambda: False, 0.2)
r["code"] = p.wait(timeout=20)
r["echo_after"] = bool(termios.tcgetattr(slave)[3] & termios.ECHO)
r["out"] = out.decode("utf-8", "replace")
print(json.dumps(r))
"#;

fn on_pty(cfg: serde_json::Value, hubd_args: &[&str], data: &Path) -> serde_json::Value {
    let o = Command::new("python3")
        .args(["-I", "-c", PTY_DRIVER, &cfg.to_string()])
        .arg(env!("CARGO_BIN_EXE_hubd"))
        .arg("--data-dir")
        .arg(data)
        .args(hubd_args)
        .env("RUST_BACKTRACE", "0")
        .env_remove("HUB_API_KEY")
        .env_remove("HUB_DATA_DIR")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

const SIGHUP: i64 = 1;
const SIGINT: i64 = 2;
const SIGQUIT: i64 = 3;
const SIGTERM: i64 = 15;

fn has(r: &serde_json::Value, at: &str, which: &str, sig: i64) -> bool {
    r[at][which]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s.as_i64() == Some(sig))
}

#[test]
fn a_signal_at_the_hidden_prompt_leaves_the_terminal_echoing() {
    if !python_available() {
        eprintln!("skipped: python3 with pty/termios not found");
        return;
    }
    let s = Scratch::new("pty-signals");
    s.ok(&["init"]);
    let prompts: [&[&str]; 2] = [
        &["secret", "set", "tunnel_token"],
        &["chat", "--url", "http://127.0.0.1:9/v1"],
    ];
    for args in prompts {
        for sig in [SIGTERM, SIGQUIT, SIGHUP] {
            let r = on_pty(
                serde_json::json!({"wait": "typing is hidden", "signal": sig}),
                args,
                &s.data(),
            );
            let what = format!("{args:?}, signal {sig}: {r}");
            assert_eq!(r["echo_at_prompt"], false, "{what}");
            assert!(has(&r, "at_prompt", "caught", sig), "{what}");
            // Ended by the signal itself, as without the handler, with echo back on.
            assert_eq!(r["code"], -sig, "{what}");
            assert_eq!(r["echo_after"], true, "echo was left off: {what}");
        }
    }
    let stored = std::fs::read_dir(s.data().join("secrets"))
        .unwrap()
        .any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("tunnel_token")
        });
    assert!(!stored, "an interrupted prompt stored a token");
}

/// Once the key is read, SIGTERM, SIGQUIT and SIGHUP have their default action again (Ctrl-C
/// stays the chat's), so `kill` ends the chat as before.
#[test]
fn signals_get_their_action_back_after_the_prompt() {
    if !python_available() {
        eprintln!("skipped: python3 with pty/termios not found");
        return;
    }
    let s = Scratch::new("pty-after");
    s.ok(&["init"]);
    let r = on_pty(
        serde_json::json!({
            "wait": "typing is hidden",
            "type": "lah_00000000_secretkeytext\n",
            "then_wait": "chatting with",
            "signal": SIGTERM,
        }),
        &[
            "chat",
            "--model",
            "m",
            "--no-history",
            "--url",
            "http://127.0.0.1:9/v1",
        ],
        &s.data(),
    );
    for sig in [SIGTERM, SIGQUIT, SIGHUP] {
        assert!(has(&r, "at_prompt", "caught", sig), "{sig}: {r}");
        assert!(!has(&r, "after_prompt", "caught", sig), "{sig}: {r}");
    }
    assert!(has(&r, "after_prompt", "caught", SIGINT), "{r}");
    assert_eq!(r["code"], -SIGTERM, "{r}");
    assert!(!r["out"].as_str().unwrap().contains("secretkeytext"), "{r}");
}

/// A SIGHUP ignored from the start (`nohup`) stays ignored while the prompt is open.
#[test]
fn an_ignored_sighup_stays_ignored_at_the_prompt() {
    if !python_available() {
        eprintln!("skipped: python3 with pty/termios not found");
        return;
    }
    let s = Scratch::new("pty-nohup");
    s.ok(&["init"]);
    let r = on_pty(
        serde_json::json!({"wait": "typing is hidden", "signal": SIGTERM, "nohup": true}),
        &["secret", "set", "tunnel_token"],
        &s.data(),
    );
    assert!(has(&r, "at_prompt", "ignored", SIGHUP), "{r}");
    assert!(!has(&r, "at_prompt", "caught", SIGHUP), "{r}");
    assert_eq!(r["code"], -SIGTERM, "{r}");
    assert_eq!(r["echo_after"], true, "{r}");
}
