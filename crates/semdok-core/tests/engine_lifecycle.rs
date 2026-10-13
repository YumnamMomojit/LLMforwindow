//! ColibriAdapter supervision against a fake launcher: a small Python HTTP server standing in
//! for upstream `c/openai_server.py` (same argv, COLI_API_KEY, "listening on" line after the
//! bind, /v1/models). Unix only (python3 from PATH); skipped when python3 is missing.
#![cfg(unix)]
use semdok_core::engine::{ColibriAdapter, ColibriConfig, EngineState, InferenceAdapter};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const KEY: &str = "internal-test-key-0123456789";

/// FAKE_MODE (read from <state>/mode): ok | die_after_ready | leader_exits_first | silent |
/// busy_once | engine_child (runs an engine process, as upstream does, and serves) | stubborn
/// (launcher and engine survive SIGTERM, never ready). The last two write "<launcher>
/// <engine>" pids to <state>/pids once the engine runs.
/// die_after_ready and leader_exits_first serve and write the launcher pid to <state>/pids;
/// once <state>/die exists they log a MemoryError, and once <state>/exit exists the launcher
/// exits with status 3. In leader_exits_first its main thread ends at "die" already, so the
/// process lives on in its other threads (a zombie in /proc that cannot be reaped yet).
const FAKE_LAUNCHER: &str = r#"
import argparse, ctypes, json, os, signal, subprocess, sys, threading, time
from http.server import BaseHTTPRequestHandler, HTTPServer
p = argparse.ArgumentParser()
for f in ("--model", "--host", "--model-id", "--max-tokens", "--engine"):
    p.add_argument(f)
p.add_argument("--port", type=int)
a = p.parse_args()
state = os.environ["FAKE_STATE"]
mode = open(os.path.join(state, "mode")).read().strip()
if mode == "stubborn":
    signal.signal(signal.SIGTERM, lambda *_: open(os.path.join(state, "term"), "a").write("x"))
if mode in ("engine_child", "stubborn"):
    ready = os.path.join(state, "engine_up")
    engine = subprocess.Popen([sys.executable, "-c",
        "import signal, sys, time\n"
        "if sys.argv[1] == 'stubborn': signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
        "open(sys.argv[2], 'w').close()\n"
        "time.sleep(600)", mode, ready])
    while not os.path.exists(ready):
        time.sleep(0.01)
    with open(os.path.join(state, "pids.tmp"), "w") as f:
        f.write(f"{os.getpid()} {engine.pid}")
    os.replace(os.path.join(state, "pids.tmp"), os.path.join(state, "pids"))
with open(os.path.join(state, "launches"), "a") as f:
    f.write(a.model_id + "\n")
launches = len(open(os.path.join(state, "launches")).read().split())
if not os.path.isfile(os.path.join(a.model, "config.json")):
    print(f"FileNotFoundError: no model at {a.model}", file=sys.stderr)
    sys.exit(4)
if mode == "busy_once" and launches == 1:
    print("OSError: [Errno 98] Address already in use", file=sys.stderr)
    sys.exit(1)
key = os.environ["COLI_API_KEY"]

class H(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        with open(os.path.join(state, "requests"), "a") as f:
            f.write(self.path + "\n")
        if self.headers.get("Authorization") != "Bearer " + key:
            self.send_response(401)
            self.end_headers()
            return
        body = json.dumps({"data": [{"id": a.model_id}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

def wait_for(name):
    while not os.path.exists(os.path.join(state, name)):
        time.sleep(0.01)

srv = HTTPServer((a.host, a.port), H)
print(f"loading with key {key}", file=sys.stderr, flush=True)
if mode == "stubborn":
    while True:
        time.sleep(1)
if mode != "silent":
    print(f"OpenAI-compatible API listening on http://{a.host}:{a.port}/v1", file=sys.stderr, flush=True)
if mode in ("die_after_ready", "leader_exits_first"):
    with open(os.path.join(state, "pids.tmp"), "w") as f:
        f.write(str(os.getpid()))
    os.replace(os.path.join(state, "pids.tmp"), os.path.join(state, "pids"))
    def end():
        wait_for("exit")
        os._exit(3)
    threading.Thread(target=end, daemon=True).start()
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    wait_for("die")
    print("MemoryError: the engine ran out of memory", file=sys.stderr, flush=True)
    if mode == "leader_exits_first":
        ctypes.CDLL(None).pthread_exit(None)
    while True:
        time.sleep(1)
srv.serve_forever()
"#;

fn python3() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("python3"))
        .find(|p| p.is_file())
}

struct Fixture {
    _dir: tempfile::TempDir,
    colibri: PathBuf,
    model: PathBuf,
    state: PathBuf,
}

fn fixture(mode: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let colibri = dir.path().join("colibri");
    std::fs::create_dir_all(colibri.join("c")).unwrap();
    let model = dir.path().join("model");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("config.json"), "{}").unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("mode"), mode).unwrap();
    // FAKE_STATE is per test, so it is baked into the script rather than set in our own
    // environment (tests run in parallel and the launcher inherits that environment).
    std::fs::write(
        colibri.join("c/openai_server.py"),
        format!(
            "import os\nos.environ['FAKE_STATE'] = {:?}\n{FAKE_LAUNCHER}",
            state.to_str().unwrap()
        ),
    )
    .unwrap();
    Fixture {
        _dir: dir,
        colibri,
        model,
        state,
    }
}

fn config(python: PathBuf, colibri: PathBuf, model: PathBuf, timeout: Duration) -> ColibriConfig {
    ColibriConfig {
        python,
        colibri_dir: colibri,
        engine_exe: None,
        model_dir: model,
        model_id: "test-model".into(),
        port: 0,
        internal_key: KEY.into(),
        startup_timeout: timeout,
        max_tokens: 16,
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// `p` (absolute) spelled relative to the current directory, via "..".
fn relative_to_cwd(p: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap();
    let mut rel = PathBuf::new();
    for c in cwd.components() {
        if matches!(c, Component::Normal(_)) {
            rel.push("..");
        }
    }
    rel.join(p.strip_prefix("/").unwrap())
}

/// Pid of the launcher, written by the die_after_ready and leader_exits_first modes.
fn launcher_pid(state: &Path) -> u32 {
    read(&state.join("pids"))
        .trim()
        .parse()
        .expect("no pid from the fake launcher")
}

/// Polls state() until Failed; every other answer must be Ready.
async fn wait_failed(e: &ColibriAdapter, launcher: u32) -> String {
    let t0 = Instant::now();
    loop {
        match e.state() {
            EngineState::Failed(m) => return m,
            EngineState::Ready => {}
            s => panic!("unexpected state {s:?}"),
        }
        if t0.elapsed() > Duration::from_secs(10) {
            unsafe {
                libc::kill(launcher as i32, libc::SIGKILL);
            }
            panic!("engine exit after Ready never reported: {:?}", e.state());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Lets the fake launcher log its MemoryError and waits until semdok-edge has read it, then lets it
/// exit, so the exit is reported with the actionable reason without any timing assumption.
/// Polls state() all along: until "exit" the launcher is still serving (or, after
/// leader_exits_first ended its main thread, cannot be reaped yet) and that is Ready.
async fn die_then_exit(e: &ColibriAdapter, f: &Fixture, launcher: u32) {
    std::fs::write(f.state.join("die"), "").unwrap();
    let t0 = Instant::now();
    while !e.log_tail().iter().any(|l| l.contains("MemoryError")) {
        assert_eq!(e.state(), EngineState::Ready);
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "no MemoryError line"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for _ in 0..10 {
        assert_eq!(e.state(), EngineState::Ready);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::write(f.state.join("exit"), "").unwrap();
    let msg = wait_failed(e, launcher).await;
    assert!(msg.contains("Not enough memory"), "{msg}");
    // The launcher's own status (3), not a tree that shrank without one.
    assert!(msg.contains("exit status: 3"), "{msg}");
    assert!(msg.contains("[redacted]"), "log tail missing: {msg}");
    assert!(!msg.contains(KEY), "internal key leaked: {msg}");
}

#[tokio::test]
async fn exit_after_ready_is_reported_as_failed() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let f = fixture("die_after_ready");
    let mut e = ColibriAdapter::new(config(
        py,
        f.colibri.clone(),
        f.model.clone(),
        Duration::from_secs(30),
    ));
    e.start().await.unwrap();
    assert_eq!(e.state(), EngineState::Ready);
    assert!(e.upstream_url().is_some());
    die_then_exit(&e, &f, launcher_pid(&f.state)).await;
    // No key goes to a port the engine no longer holds.
    assert_eq!(e.upstream_url(), None);
    assert!(!e.health().await);
    assert!(matches!(e.state(), EngineState::Failed(_)));
    e.stop().await;
    assert_eq!(e.state(), EngineState::NotConfigured);
}

/// A launcher whose main thread has ended shows as a zombie in /proc, so the tree count no
/// longer has it, but it cannot be reaped until its other threads end. It must not be taken
/// for a tree that lost its engine (Failed without the exit status, and a launcher killed
/// that was still serving): it is reported once it exits, with its own status. Here the
/// state lasts until the test ends the process; a multi-threaded launcher passes through it
/// for a moment whenever it exits.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn launcher_that_cannot_be_reaped_yet_is_reported_with_its_status() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let f = fixture("leader_exits_first");
    let mut e = ColibriAdapter::new(config(
        py,
        f.colibri.clone(),
        f.model.clone(),
        Duration::from_secs(30),
    ));
    e.start().await.unwrap();
    let launcher = launcher_pid(&f.state);
    assert!(alive(launcher));
    std::fs::write(f.state.join("die"), "").unwrap();
    let t0 = Instant::now();
    while alive(launcher) {
        assert_eq!(e.state(), EngineState::Ready);
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "main thread never ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Its main thread is a zombie now, the process lives on and still serves.
    assert!(Path::new(&format!("/proc/{launcher}")).exists());
    for _ in 0..10 {
        assert_eq!(e.state(), EngineState::Ready);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(e.health().await);
    die_then_exit(&e, &f, launcher).await;
    assert_eq!(e.upstream_url(), None);
    e.stop().await;
    assert_eq!(e.state(), EngineState::NotConfigured);
}

/// Gone, or a zombie nobody reaped yet: dead.
#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(s) => !matches!(
            s.rsplit_once(')')
                .and_then(|(_, r)| r.trim_start().chars().next()),
            Some('Z' | 'X')
        ),
    }
}

/// (launcher, engine) pids written by the engine_child and stubborn modes.
#[cfg(target_os = "linux")]
fn tree_pids(state: &Path) -> Option<(u32, u32)> {
    let s = std::fs::read_to_string(state.join("pids")).ok()?;
    let mut p = s.split_whitespace().map(|p| p.parse().unwrap());
    Some((p.next()?, p.next()?))
}

#[cfg(target_os = "linux")]
async fn wait_dead(pids: &[u32]) {
    let t0 = Instant::now();
    while pids.iter().any(|&p| alive(p)) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let left: Vec<_> = pids.iter().filter(|&&p| alive(p)).collect();
    for &&p in &left {
        unsafe {
            libc::kill(p as i32, libc::SIGKILL);
        }
    }
    assert!(left.is_empty(), "engine processes survived: {left:?}");
}

/// A process outside the engine tree whose name in /proc/<pid>/stat is not UTF-8, as the
/// kernel's 15-byte cut of a name like "ééééééééé" is. Killed on drop.
#[cfg(target_os = "linux")]
struct OddName(std::process::Child);

#[cfg(target_os = "linux")]
fn odd_name() -> OddName {
    use std::io::BufRead;
    use std::process::Stdio;
    let mut c = std::process::Command::new("sh")
        .args(["-c", r"printf '\303' > /proc/$$/comm && echo ok && read x"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(c.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let c = OddName(c);
    assert_eq!(line.trim(), "ok", "could not rename the process");
    let stat = std::fs::read(format!("/proc/{}/stat", c.0.id())).unwrap();
    assert!(std::str::from_utf8(&stat).is_err(), "name is UTF-8");
    c
}

#[cfg(target_os = "linux")]
impl Drop for OddName {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Upstream's launcher outlives its engine: when the engine is OOM-killed or crashes, it
/// keeps answering /v1/models. That must still be reported, and the launcher stopped.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn engine_exit_after_ready_is_reported_while_the_launcher_lives() {
    engine_exit_while_the_launcher_lives(false).await;
}

/// The same with a process on the host whose /proc entry is not UTF-8, for the whole run:
/// the tree count must skip it, not give up (which left the engine Ready for good).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn engine_exit_is_reported_with_a_non_utf8_process_name_on_the_host() {
    engine_exit_while_the_launcher_lives(true).await;
}

#[cfg(target_os = "linux")]
async fn engine_exit_while_the_launcher_lives(odd_name_on_host: bool) {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let _odd = odd_name_on_host.then(odd_name);
    let f = fixture("engine_child");
    let mut e = ColibriAdapter::new(config(
        py,
        f.colibri.clone(),
        f.model.clone(),
        Duration::from_secs(30),
    ));
    e.start().await.unwrap();
    let (launcher, engine) = tree_pids(&f.state).expect("no pids from the fake launcher");
    // A whole tree stays Ready.
    for _ in 0..10 {
        assert_eq!(e.state(), EngineState::Ready);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(e.health().await);
    unsafe {
        libc::kill(engine as i32, libc::SIGKILL);
    }
    let t0 = Instant::now();
    let msg = loop {
        if let EngineState::Failed(m) = e.state() {
            break m;
        }
        if t0.elapsed() > Duration::from_secs(10) {
            unsafe {
                libc::kill(launcher as i32, libc::SIGKILL);
            }
            panic!("engine exit after Ready never reported: {:?}", e.state());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(msg.contains("exited unexpectedly"), "{msg}");
    assert!(!msg.contains(KEY), "internal key leaked: {msg}");
    assert_eq!(e.upstream_url(), None);
    assert!(!e.health().await);
    // The launcher, useless without its engine, is stopped at once, not at stop().
    wait_dead(&[launcher, engine]).await;
    e.stop().await;
    assert_eq!(e.state(), EngineState::NotConfigured);
}

/// semdok-edge races start() against a stop request; one that lands while the startup-timeout kill
/// is waiting for SIGTERM to work drops start() mid-kill. stop() must still end the tree.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn stop_after_start_was_dropped_mid_teardown_ends_the_tree() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let f = fixture("stubborn");
    let mut e = ColibriAdapter::new(config(
        py,
        f.colibri.clone(),
        f.model.clone(),
        Duration::from_secs(2),
    ));
    {
        let start = e.start();
        tokio::pin!(start);
        let t0 = Instant::now();
        // Until the launcher got the startup-timeout SIGTERM: kill_tree then waits up to 5 s.
        while !(f.state.join("term").exists() && tree_pids(&f.state).is_some()) {
            tokio::select! {
                r = &mut start => panic!("start() ended before it was dropped: {r:?}"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "no startup timeout");
        }
    }
    let (launcher, engine) = tree_pids(&f.state).expect("no pids from the fake launcher");
    e.stop().await;
    assert_eq!(e.state(), EngineState::NotConfigured);
    wait_dead(&[launcher, engine]).await;
}

#[tokio::test]
async fn relative_colibri_and_model_dirs_work() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let f = fixture("ok");
    let (colibri, model) = (relative_to_cwd(&f.colibri), relative_to_cwd(&f.model));
    assert!(colibri.is_relative() && colibri.join("c/openai_server.py").is_file());
    let mut e = ColibriAdapter::new(config(py, colibri, model, Duration::from_secs(30)));
    let r = e.start().await;
    e.stop().await;
    r.unwrap();
}

#[tokio::test]
async fn key_is_not_sent_before_the_launcher_reports_listening() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    // The fake binds and would answer, but never says it is listening: a stand-in for a port
    // owned by some other process. Nothing may be sent to it.
    let f = fixture("silent");
    let mut e = ColibriAdapter::new(config(
        py,
        f.colibri.clone(),
        f.model.clone(),
        Duration::from_secs(3),
    ));
    let err = e.start().await.unwrap_err();
    assert!(err.contains("did not become ready"), "{err}");
    assert_eq!(read(&f.state.join("requests")), "", "a request was sent");
    assert_eq!(e.upstream_url(), None);
    assert!(matches!(e.state(), EngineState::Failed(_)));
}

#[tokio::test]
async fn port_taken_before_bind_is_retried_on_a_fresh_port() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let f = fixture("busy_once");
    let mut e = ColibriAdapter::new(config(
        py,
        f.colibri.clone(),
        f.model.clone(),
        Duration::from_secs(30),
    ));
    let r = e.start().await;
    e.stop().await;
    r.unwrap();
    assert_eq!(read(&f.state.join("launches")).lines().count(), 2);
}

#[tokio::test]
async fn model_id_starting_with_dash_reaches_the_launcher_as_a_value() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let f = fixture("ok");
    let mut cfg = config(
        py,
        f.colibri.clone(),
        f.model.clone(),
        Duration::from_secs(30),
    );
    cfg.model_id = "-legacy".into();
    let mut e = ColibriAdapter::new(cfg);
    let r = e.start().await;
    e.stop().await;
    r.unwrap();
    assert_eq!(read(&f.state.join("launches")).trim(), "-legacy");
}
