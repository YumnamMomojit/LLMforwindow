//! Local-owner commands through the real binary: history arguments are checked before the
//! database is touched, unknown conversations are errors, workspace exports never replace a
//! file unasked, `workspace put` refuses a FIFO without hanging, workspace commands wait for
//! each other, and secrets typed at a terminal are not echoed (driven through a pty by
//! python3; skipped without it). Unix only.
#![cfg(unix)]
use fs4::fs_std::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cli2-local-{tag}-{}-{}",
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
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_semdok-edge"));
        c.arg("--data-dir")
            .arg(self.data())
            .args(args)
            .env("RUST_BACKTRACE", "0")
            .env_remove("SEMDOK_DATA_DIR")
            .stdin(Stdio::null());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into()
}

#[test]
fn history_arguments_are_checked_before_the_database_is_opened() {
    let s = Scratch::new("hist-args");
    for (args, msg) in [
        (&["history", "bogus"][..], "unknown history subcommand"),
        (&["history", "list", "extra"], "unexpected argument 'extra'"),
        (
            &["history", "retention", "0"],
            "retention must be a whole number",
        ),
        (&["history", "show"], "missing conversation id"),
    ] {
        let o = s.run(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(stderr(&o).contains(msg), "{args:?}: {}", stderr(&o));
    }
    assert!(!s.data().exists(), "a usage error created the data dir");
}

#[test]
fn history_show_of_an_unknown_conversation_is_an_error() {
    let s = Scratch::new("hist-show");
    s.ok(&["init"]);
    let o = s.run(&["history", "show", "0123456789abcdef"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("no conversation with id"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn workspace_export_never_replaces_a_file_unasked() {
    let s = Scratch::new("export");
    s.ok(&["init"]);
    s.ok(&["workspace", "create", "A"]);
    let src = s.0.join("note.txt");
    std::fs::write(&src, "hello").unwrap();
    s.ok(&["workspace", "put", "A", src.to_str().unwrap()]);
    let dest = s.0.join("precious.zip");
    std::fs::write(&dest, "keep me").unwrap();
    let o = s.run(&["workspace", "export", "A", dest.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("already exists"), "{}", stderr(&o));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "keep me");
    s.ok(&[
        "workspace",
        "export",
        "A",
        dest.to_str().unwrap(),
        "--overwrite",
    ]);
    assert!(std::fs::read(&dest).unwrap().starts_with(b"PK"));
}

#[test]
fn workspace_put_refuses_a_fifo_without_waiting_for_a_writer() {
    let s = Scratch::new("fifo");
    s.ok(&["init"]);
    s.ok(&["workspace", "create", "A"]);
    let fifo = s.0.join("pipe");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: plain mkfifo(3) on a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let mut child = s
        .cmd(&["workspace", "put", "A", fifo.to_str().unwrap()])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if t0.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("workspace put is blocked on the FIFO");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
    let o = child.wait_with_output().unwrap();
    assert!(stderr(&o).contains("not a regular file"), "{}", stderr(&o));
}

/// While one workspace command runs (here: the test holds its lock), another waits instead of
/// clearing the running one's upload in progress.
#[test]
fn workspace_commands_wait_for_each_other() {
    let s = Scratch::new("lock");
    s.ok(&["init"]);
    s.ok(&["workspace", "create", "A"]);
    let id = s
        .ok(&["workspace", "list"])
        .split('\t')
        .next()
        .unwrap()
        .to_string();
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(s.data().join("workspaces.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    // What a `workspace put` that is still writing has on disk.
    let upload = s.data().join("workspaces").join(&id).join(".tmp-uploads");
    std::fs::create_dir_all(&upload).unwrap();
    std::fs::write(upload.join("in-progress"), "half").unwrap();
    let mut child = s
        .cmd(&["workspace", "files", "A"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(child.try_wait().unwrap().is_none(), "it did not wait");
    assert!(
        upload.join("in-progress").exists(),
        "the running upload was deleted"
    );
    drop(lock);
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("waiting for another workspace"),
        "{}",
        stderr(&o)
    );
}

fn python_available() -> bool {
    Command::new("python3")
        .args(["-c", "import pty, termios"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs semdok-edge on a fresh pty: waits for `prompt`, records whether echo is off, types `input`,
/// collects all output until semdok-edge exits, then reads the terminal's echo flag again.
const PTY_DRIVER: &str = r#"
import fcntl, json, os, select, subprocess, sys, termios, time
prompt, data = sys.argv[1].encode(), sys.argv[2].encode()
master, slave = os.openpty()
def setup():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
p = subprocess.Popen(sys.argv[3:], stdin=slave, stdout=slave, stderr=slave, preexec_fn=setup)
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
pump(lambda: prompt in out, 20)
time.sleep(0.3)
echo_during = bool(termios.tcgetattr(slave)[3] & termios.ECHO)
os.write(master, data)
pump(lambda: p.poll() is not None, 20)
pump(lambda: False, 0.3)
code = p.wait(timeout=20)
print(json.dumps({"out": out.decode("utf-8", "replace"), "echo_during": echo_during,
                  "echo_after": bool(termios.tcgetattr(slave)[3] & termios.ECHO), "code": code}))
"#;

fn on_pty(prompt: &str, input: &str, semdok_args: &[&str], data: &Path) -> serde_json::Value {
    let o = Command::new("python3")
        .args([
            "-I",
            "-c",
            PTY_DRIVER,
            prompt,
            input,
            env!("CARGO_BIN_EXE_semdok-edge"),
        ])
        .arg("--data-dir")
        .arg(data)
        .args(semdok_args)
        .env("RUST_BACKTRACE", "0")
        .env_remove("SEMDOK_API_KEY")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[test]
fn a_token_typed_at_a_terminal_is_not_echoed() {
    if !python_available() {
        eprintln!("skipped: python3 with pty/termios not found");
        return;
    }
    let s = Scratch::new("pty-token");
    s.ok(&["init"]);
    let token = "s3cret-tunnel-token";
    let r = on_pty(
        "typing is hidden",
        &format!("{token}\n"),
        &["secret", "set", "tunnel_token"],
        &s.data(),
    );
    assert_eq!(r["code"], 0, "{r}");
    let out = r["out"].as_str().unwrap();
    assert!(out.contains("tunnel token (typing is hidden)"), "{out}");
    assert!(out.contains("stored"), "{out}");
    assert!(!out.contains(token), "the token was echoed: {out}");
    assert_eq!(r["echo_during"], false, "{r}");
    assert_eq!(r["echo_after"], true, "echo was not restored: {r}");
    let stored = std::fs::read_dir(s.data().join("secrets"))
        .unwrap()
        .any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("tunnel_token")
        });
    assert!(stored);
}

#[test]
fn ctrl_c_at_the_hidden_prompt_cancels_and_restores_echo() {
    if !python_available() {
        eprintln!("skipped: python3 with pty/termios not found");
        return;
    }
    let s = Scratch::new("pty-cancel");
    s.ok(&["init"]);
    let r = on_pty(
        "typing is hidden",
        "half-typed\x03",
        &["secret", "set", "tunnel_token"],
        &s.data(),
    );
    assert_eq!(r["code"], 1, "{r}");
    assert!(r["out"].as_str().unwrap().contains("cancelled"), "{r}");
    assert!(!r["out"].as_str().unwrap().contains("half-typed"), "{r}");
    assert_eq!(r["echo_after"], true, "echo was not restored: {r}");
    let stored = std::fs::read_dir(s.data().join("secrets"))
        .unwrap()
        .any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("tunnel_token")
        });
    assert!(!stored, "a cancelled prompt stored a token");
}

#[test]
fn the_chat_key_prompt_is_hidden_too() {
    if !python_available() {
        eprintln!("skipped: python3 with pty/termios not found");
        return;
    }
    let s = Scratch::new("pty-key");
    // Nothing listens on this port: the chat stops right after reading the key.
    let r = on_pty(
        "typing is hidden",
        "sem_00000000_secretkeytext\n",
        &["chat", "--url", "http://127.0.0.1:9/v1"],
        &s.data(),
    );
    let out = r["out"].as_str().unwrap();
    assert_eq!(r["code"], 1, "{r}");
    assert!(out.contains("cannot reach"), "{out}");
    assert!(!out.contains("secretkeytext"), "the key was echoed: {out}");
    assert_eq!(r["echo_during"], false, "{r}");
    assert_eq!(r["echo_after"], true, "{r}");
}
