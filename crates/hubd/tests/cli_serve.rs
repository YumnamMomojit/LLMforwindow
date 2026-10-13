//! `hubd serve` lifecycle against a fake Colibri launcher (Python; skipped when `python3` is
//! missing). The fake starts a child that stands in for the C engine, in the launcher's
//! process group like the real one, and records both pids. Checked: every stop request
//! (SIGINT, SIGTERM, SIGHUP) during a model load and while serving tears the whole tree down;
//! a SIGHUP ignored from the start (nohup) stays ignored;
//! failures that do not need the engine are reported before it starts; engine start failures
//! exit nonzero with the actionable message; `--max-output-tokens` reaches both the engine
//! and the gateway, and `hubd chat` marks a reply cut there. Linux only (process liveness is read from /proc);
//! on Windows the Job Object owns the tree and the console events are not tested here.
#![cfg(target_os = "linux")]
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FAMILY_REGISTRY: &str = r#"
# Stub of the upstream family registry: every folder is a text chat family.
from types import SimpleNamespace

def resolve_model(model_dir):
    d = SimpleNamespace(id="stubchat", display_name="Stub chat", modality="text",
                        tokenizer_file="tokenizer.json",
                        capabilities=SimpleNamespace(decision=False, image=False))
    return SimpleNamespace(descriptor=d, model_type="stub", config={}, family_config={},
                           model_dir=str(model_dir), decision_head=None)
"#;

const LAUNCHER: &str = r#"
# Fake c/openai_server.py. FAKE_ENGINE_MODE: crash | hang (endless model load) | serve.
import argparse, json, os, subprocess, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer

# argparse, like upstream: accepts both "--opt value" and "--opt=value".
ap = argparse.ArgumentParser()
for opt in ("--model", "--host", "--port", "--model-id", "--max-tokens", "--engine"):
    ap.add_argument(opt)
args = {f"--{k.replace('_', '-')}": v for k, v in vars(ap.parse_args()).items()}
mode = os.environ["FAKE_ENGINE_MODE"]
if mode == "crash":
    print("MemoryError: cannot allocate the expert cache", file=sys.stderr)
    sys.exit(1)
engine = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(600)"])
pids = os.environ["FAKE_ENGINE_PIDS"]
with open(pids + ".args", "w") as f:  # complete before the pids file appears
    json.dump(args, f)
with open(pids + ".tmp", "w") as f:
    f.write(f"{os.getpid()} {engine.pid}")
os.replace(pids + ".tmp", pids)
if mode == "hang":
    time.sleep(600)

class Models(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": args["--model-id"], "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # Streams "max_tokens=N" (the limit it was sent), then stops there as a cut reply does:
    # finish_reason "length" and, if asked for, usage with N completion tokens.
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        n = body.get("max_tokens")
        def event(obj):
            self.wfile.write(("data: " + json.dumps(obj) + "\n\n").encode())
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        event({"choices": [{"index": 0, "delta": {"content": f"max_tokens={n}"}, "finish_reason": None}]})
        event({"choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]})
        if (body.get("stream_options") or {}).get("include_usage"):
            event({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": n, "total_tokens": n + 1}})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def log_message(self, *a):
        pass

srv = HTTPServer(("127.0.0.1", int(args["--port"])), Models)
# Same line, same stream, same moment (after bind) as upstream openai_server.py serve().
print(f"OpenAI-compatible API listening on http://127.0.0.1:{args['--port']}/v1", file=sys.stderr, flush=True)
srv.serve_forever()
"#;

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    colibri: PathBuf,
    model: PathBuf,
    pids: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn python_available() -> bool {
    Command::new("python3")
        .args(["-c", "pass"])
        .status()
        .is_ok_and(|s| s.success())
}

fn hubd(data: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hubd"));
    c.arg("--data-dir")
        .arg(data)
        .env("RUST_BACKTRACE", "0")
        .env_remove("HUB_DATA_DIR")
        .env_remove("HUB_PYTHON")
        .env_remove("HUB_COLIBRI_DIR")
        .stdin(Stdio::null());
    c
}

fn ok(c: &mut Command) -> String {
    let o = c.output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into()
}

/// A data dir with model `stub` registered through `hubd model add` and the stub registry.
fn fixture(tag: &str) -> Option<Fixture> {
    if !python_available() {
        eprintln!("skipped: python3 not found");
        return None;
    }
    static N: AtomicUsize = AtomicUsize::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "cli-serve-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let f = Fixture {
        data: root.join("data"),
        colibri: root.join("colibri"),
        model: root.join("model"),
        pids: root.join("engine.pids"),
        root,
    };
    std::fs::create_dir_all(f.colibri.join("c")).unwrap();
    std::fs::create_dir_all(&f.model).unwrap();
    std::fs::write(f.colibri.join("c/family_registry.py"), FAMILY_REGISTRY).unwrap();
    std::fs::write(f.colibri.join("c/openai_server.py"), LAUNCHER).unwrap();
    std::fs::write(f.model.join("config.json"), r#"{"model_type":"stub"}"#).unwrap();
    std::fs::write(f.model.join("tokenizer.json"), "{}").unwrap();
    std::fs::write(f.model.join("model.safetensors"), [0u8; 16]).unwrap();
    ok(hubd(&f.data).arg("init"));
    let added = ok(hubd(&f.data)
        .args(["model", "add"])
        .arg(&f.model)
        .args(["--id", "stub", "--python", "python3", "--colibri-dir"])
        .arg(&f.colibri));
    assert!(added.contains("added"), "{added}");
    Some(f)
}

/// A running `hubd serve` with its output collected. Dropping it kills whatever is left, so
/// a failed assertion does not leak processes.
struct Served {
    child: Child,
    out: Arc<Mutex<String>>,
    err: Arc<Mutex<String>>,
    pids: PathBuf,
}

fn collect(r: impl Read + Send + 'static, buf: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut r = BufReader::new(r);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            buf.lock().unwrap().push_str(&line);
            line.clear();
        }
    });
}

impl Fixture {
    fn serve(&self, mode: &str, port: u16, extra: &[&str]) -> Served {
        self.serve_with(mode, port, extra, false)
    }

    /// `ignore_hup`: start hubd with SIGHUP already ignored, as `nohup` does; otherwise with
    /// the default disposition, even if the test run itself was started under nohup.
    fn serve_with(&self, mode: &str, port: u16, extra: &[&str], ignore_hup: bool) -> Served {
        use std::os::unix::process::CommandExt;
        let mut cmd = hubd(&self.data);
        let disposition = if ignore_hup {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        // SAFETY: only the async-signal-safe signal(2) runs between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                libc::signal(libc::SIGHUP, disposition);
                Ok(())
            });
        }
        let mut child = cmd
            .args([
                "serve",
                "--model",
                "stub",
                "--python",
                "python3",
                "--colibri-dir",
            ])
            .arg(&self.colibri)
            .args(["--port", &port.to_string()])
            .args(extra)
            .env("FAKE_ENGINE_MODE", mode)
            .env("FAKE_ENGINE_PIDS", &self.pids)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (out, err) = (Arc::default(), Arc::default());
        collect(child.stdout.take().unwrap(), Arc::clone(&out));
        collect(child.stderr.take().unwrap(), Arc::clone(&err));
        Served {
            child,
            out,
            err,
            pids: self.pids.clone(),
        }
    }
}

fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// Running, i.e. neither gone nor a zombie waiting to be reaped.
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !matches!(
            stat.rsplit_once(") ")
                .and_then(|(_, rest)| rest.chars().next()),
            Some('Z' | 'X')
        ),
        Err(_) => false,
    }
}

impl Served {
    fn out(&self) -> String {
        self.out.lock().unwrap().clone()
    }
    fn err(&self) -> String {
        self.err.lock().unwrap().clone()
    }
    fn engine_pids(&self) -> Option<Vec<u32>> {
        let s = std::fs::read_to_string(&self.pids).ok()?;
        Some(s.split_whitespace().map(|p| p.parse().unwrap()).collect())
    }
    fn wait_for_engine(&self) -> Vec<u32> {
        assert!(
            wait_until(Duration::from_secs(30), || self.engine_pids().is_some()),
            "the fake engine never started\nstdout: {}\nstderr: {}",
            self.out(),
            self.err()
        );
        self.engine_pids().unwrap()
    }
    fn signal(&self, sig: &str) {
        let s = Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(s.success());
    }
    fn wait_exit(&mut self) -> ExitStatus {
        let mut status = None;
        wait_until(Duration::from_secs(40), || {
            status = self.child.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap_or_else(|| {
            panic!(
                "hubd did not exit\nstdout: {}\nstderr: {}",
                self.out(),
                self.err()
            )
        })
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(p) = self.engine_pids() {
            for pid in p.into_iter().filter(|&p| alive(p)) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

fn assert_tree_gone(pids: &[u32]) {
    assert!(
        wait_until(Duration::from_secs(10), || pids.iter().all(|&p| !alive(p))),
        "engine processes survived hubd: {:?}",
        pids.iter().filter(|&&p| alive(p)).collect::<Vec<_>>()
    );
}

/// The stop request arrives while the engine is still loading (it never becomes ready).
fn stop_during_startup(sig: &str) {
    let Some(f) = fixture(&format!("startup-{sig}")) else {
        return;
    };
    let mut s = f.serve("hang", 0, &[]);
    let pids = s.wait_for_engine();
    s.signal(sig);
    let status = s.wait_exit();
    assert!(status.success(), "{status}\n{}", s.err());
    assert!(s.out().contains("during engine startup"), "{}", s.out());
    assert_tree_gone(&pids);
}

/// The stop request arrives after the engine is ready and the local API is up.
fn stop_while_serving(sig: &str) {
    let Some(f) = fixture(&format!("serving-{sig}")) else {
        return;
    };
    let mut s = f.serve("serve", 0, &[]);
    let pids = s.wait_for_engine();
    assert!(
        wait_until(Duration::from_secs(30), || s
            .out()
            .contains("Ctrl-C to stop")),
        "not ready\nstdout: {}\nstderr: {}",
        s.out(),
        s.err()
    );
    assert!(s.out().contains("sharing: OFF"), "{}", s.out());
    s.signal(sig);
    let status = s.wait_exit();
    assert!(status.success(), "{status}\n{}", s.err());
    assert!(s.out().contains(&format!("{sig} received")), "{}", s.out());
    assert_tree_gone(&pids);
}

#[test]
fn sigint_during_startup_stops_the_engine_tree() {
    stop_during_startup("INT");
}

#[test]
fn sigterm_during_startup_stops_the_engine_tree() {
    stop_during_startup("TERM");
}

#[test]
fn sighup_during_startup_stops_the_engine_tree() {
    stop_during_startup("HUP");
}

#[test]
fn sigint_while_serving_stops_the_engine_tree() {
    stop_while_serving("INT");
}

#[test]
fn sigterm_while_serving_stops_the_engine_tree() {
    stop_while_serving("TERM");
}

#[test]
fn sighup_while_serving_stops_the_engine_tree() {
    stop_while_serving("HUP");
}

/// The SIGHUP disposition hubd currently has, from /proc: ignored, caught, or default.
fn sighup_disposition(pid: u32) -> &'static str {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let mask = |name: &str| {
        let v = status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap()
            .trim();
        u64::from_str_radix(v, 16).unwrap()
    };
    if mask("SigIgn:") & 1 != 0 {
        "ignored"
    } else if mask("SigCgt:") & 1 != 0 {
        "caught"
    } else {
        "default"
    }
}

/// `nohup hubd serve &`: an inherited SIG_IGN for SIGHUP is kept, so a hangup (logout, closed
/// SSH session) does not stop serving; SIGTERM still stops everything.
#[test]
fn sighup_ignored_by_nohup_stays_ignored() {
    let Some(f) = fixture("nohup") else {
        return;
    };
    let mut s = f.serve_with("serve", 0, &[], true);
    let pids = s.wait_for_engine();
    assert!(
        wait_until(Duration::from_secs(30), || s
            .out()
            .contains("Ctrl-C to stop")),
        "not ready\nstdout: {}\nstderr: {}",
        s.out(),
        s.err()
    );
    s.signal("HUP");
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        s.child.try_wait().unwrap().is_none(),
        "hubd stopped on an ignored SIGHUP\nstdout: {}",
        s.out()
    );
    assert!(!s.out().contains("SIGHUP received"), "{}", s.out());
    assert!(pids.iter().all(|&p| alive(p)), "the engine was stopped");
    assert_eq!(sighup_disposition(s.child.id()), "ignored");
    assert!(s.out().contains("SIGHUP is ignored"), "{}", s.out());
    s.signal("TERM");
    let status = s.wait_exit();
    assert!(status.success(), "{status}\n{}", s.err());
    assert_tree_gone(&pids);
}

/// Without nohup, hubd catches SIGHUP (and stops on it: sighup_while_serving_...).
#[test]
fn sighup_is_caught_when_not_ignored() {
    let Some(f) = fixture("hup-caught") else {
        return;
    };
    let mut s = f.serve("serve", 0, &[]);
    let pids = s.wait_for_engine();
    assert!(
        wait_until(Duration::from_secs(30), || s
            .out()
            .contains("Ctrl-C to stop")),
        "not ready\nstdout: {}\nstderr: {}",
        s.out(),
        s.err()
    );
    assert_eq!(sighup_disposition(s.child.id()), "caught");
    assert!(!s.out().contains("SIGHUP is ignored"), "{}", s.out());
    s.signal("TERM");
    assert!(s.wait_exit().success());
    assert_tree_gone(&pids);
}

#[test]
fn startup_timeout_flag_is_honoured_and_stops_the_engine() {
    let Some(f) = fixture("timeout") else {
        return;
    };
    let mut s = f.serve("hang", 0, &["--startup-timeout", "2"]);
    let pids = s.wait_for_engine();
    let status = s.wait_exit();
    assert_eq!(status.code(), Some(1), "{}", s.err());
    assert!(s.err().contains("within 2 s"), "{}", s.err());
    assert_tree_gone(&pids);
}

#[test]
fn engine_start_failure_is_actionable_and_exits_nonzero() {
    let Some(f) = fixture("crash") else {
        return;
    };
    let mut s = f.serve("crash", 0, &[]);
    let status = s.wait_exit();
    assert_eq!(status.code(), Some(1), "{}", s.err());
    assert!(s.err().contains("engine failed"), "{}", s.err());
    assert!(s.err().contains("Not enough memory"), "{}", s.err());
}

#[test]
fn taken_port_is_reported_before_the_engine_starts() {
    let Some(f) = fixture("port") else {
        return;
    };
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut s = f.serve("hang", port, &[]);
    let status = s.wait_exit();
    assert_eq!(status.code(), Some(1), "{}", s.err());
    assert!(
        s.err()
            .contains(&format!("port {port} on 127.0.0.1 is not available")),
        "{}",
        s.err()
    );
    assert!(s.engine_pids().is_none(), "the engine was started");
}

fn share_args(cloudflared: &Path) -> Vec<String> {
    vec![
        "--share".to_string(),
        "--cloudflared".into(),
        cloudflared.to_string_lossy().into_owned(),
        "--hostname".into(),
        "api.example.invalid".into(),
        "--i-authorize-public-sharing".into(),
    ]
}

fn set_token(f: &Fixture, token: &str) {
    let o = hubd(&f.data)
        .args(["secret", "set", "tunnel_token"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{token}\n").as_bytes())?;
            c.wait()
        })
        .unwrap();
    assert!(o.success());
}

#[test]
fn sharing_problems_are_reported_before_the_engine_starts() {
    let Some(f) = fixture("share") else {
        return;
    };
    let exists = f.model.join("config.json");
    let run = |extra: Vec<String>, expect: &str| {
        let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
        let mut s = f.serve("hang", 0, &extra);
        let status = s.wait_exit();
        assert_eq!(status.code(), Some(1), "{}", s.err());
        assert!(
            s.err().contains(expect),
            "expected '{expect}' in {}",
            s.err()
        );
        assert!(s.engine_pids().is_none(), "the engine was started");
    };
    run(share_args(&exists), "no tunnel token stored");
    // An empty token (stored by an older hubd) is refused rather than handed to cloudflared.
    std::fs::write(f.data.join("secrets/tunnel_token.secret"), b"").unwrap();
    run(share_args(&exists), "tunnel token is empty");
    set_token(&f, "tok");
    run(
        share_args(&f.root.join("no-such-cloudflared")),
        "cloudflared not found",
    );
}

/// A fake cloudflared that logs a registered connection: hubd reports CONNECTING, then
/// CONNECTED, never "LOCAL ONLY", never the token; SIGTERM stops tunnel and engine.
#[test]
fn sharing_state_is_reported_and_stops_with_hubd() {
    use std::os::unix::fs::PermissionsExt;
    let Some(f) = fixture("connected") else {
        return;
    };
    let token = "tok-0123456789-secret";
    set_token(&f, token);
    let fake = f.root.join("cloudflared");
    let pid_file = f.root.join("cloudflared.pid");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho $$ > '{}'\necho \"INF Registered tunnel connection connIndex=0 token=$TUNNEL_TOKEN\" >&2\nexec sleep 600\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    /// Kills the fake cloudflared if an assertion fails before hubd could stop it.
    struct Reap(PathBuf);
    impl Drop for Reap {
        fn drop(&mut self) {
            if let Ok(pid) = std::fs::read_to_string(&self.0) {
                let _ = Command::new("kill")
                    .args(["-KILL", pid.trim()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
    let _reap = Reap(pid_file.clone());
    let extra = share_args(&fake);
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    let mut s = f.serve("serve", 0, &extra);
    let pids = s.wait_for_engine();
    assert!(
        wait_until(Duration::from_secs(30), || s
            .out()
            .contains("sharing: CONNECTED")),
        "stdout: {}\nstderr: {}",
        s.out(),
        s.err()
    );
    let out = s.out();
    assert!(
        out.contains("sharing: CONNECTING https://api.example.invalid/v1"),
        "{out}"
    );
    assert!(
        !out.contains("LOCAL ONLY") && !out.contains("sharing: OFF"),
        "{out}"
    );
    let tunnel: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    s.signal("TERM");
    let status = s.wait_exit();
    assert!(status.success(), "{status}\n{}", s.err());
    assert!(!s.out().contains(token) && !s.err().contains(token));
    assert_tree_gone(&[pids, vec![tunnel]].concat());
}

impl Served {
    /// The local API's base URL, once serving.
    fn api_url(&self) -> String {
        assert!(
            wait_until(Duration::from_secs(30), || self
                .out()
                .contains("Ctrl-C to stop")),
            "not ready\nstdout: {}\nstderr: {}",
            self.out(),
            self.err()
        );
        let out = self.out();
        let rest = &out[out.find("local API ready:").unwrap()..];
        let url = rest.split_whitespace().nth(3).unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{out}");
        url.to_string()
    }

    /// The arguments the fake launcher was started with.
    fn engine_args(&self) -> serde_json::Value {
        let p = format!("{}.args", self.pids.display());
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }
}

/// `--max-output-tokens` is the engine's `--max-tokens` and the gateway's cap; `hubd chat`
/// asking for more is told that the server lowered it.
#[test]
fn max_output_tokens_caps_engine_and_gateway_and_chat_says_so() {
    let Some(f) = fixture("max-out") else {
        return;
    };
    let created = ok(hubd(&f.data).args(["key", "create", "t"]));
    let key = created.lines().last().unwrap().trim().to_string();
    assert!(key.starts_with("lah_"), "{created}");
    let mut s = f.serve("serve", 0, &["--max-output-tokens", "77"]);
    s.wait_for_engine();
    let url = s.api_url();
    assert_eq!(s.engine_args()["--max-tokens"], "77");
    assert!(s.out().contains("replies: up to 77 tokens"), "{}", s.out());
    let chat = |extra: &[&str]| {
        let mut child = hubd(&f.data)
            .args(["chat", "--no-history", "--url", &url])
            .args(extra)
            .env("HUB_API_KEY", &key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
        let o = child.wait_with_output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let cut = "[reply stopped at the token limit; use --max-tokens N, up to the server limit]";
    // More than the cap: the engine is sent the cap, and chat names the lowered limit.
    let out = chat(&["--max-tokens", "5000"]);
    assert!(out.contains("max_tokens=77"), "{out}");
    assert!(out.contains(cut), "{out}");
    assert!(
        out.contains("the reply stopped after 77 of the 5000 tokens asked for"),
        "{out}"
    );
    // No --max-tokens: the server's default, itself within the cap.
    let out = chat(&[]);
    assert!(out.contains("max_tokens=77"), "{out}");
    assert!(out.contains(cut) && !out.contains("fewer than"), "{out}");
    s.signal("TERM");
    assert!(s.wait_exit().success(), "{}", s.err());
}

#[test]
fn max_output_tokens_defaults_to_the_gateway_default() {
    let Some(f) = fixture("max-out-default") else {
        return;
    };
    let mut s = f.serve("serve", 0, &[]);
    s.wait_for_engine();
    s.api_url();
    let d = hub_core::gateway::GatewayConfig::default();
    assert_eq!(
        s.engine_args()["--max-tokens"],
        d.max_output_tokens.to_string()
    );
    assert!(
        s.out().contains(&format!(
            "replies: up to {} tokens (--max-output-tokens); {} when a request sets no max_tokens",
            d.max_output_tokens,
            d.default_output_tokens.min(d.max_output_tokens)
        )),
        "{}",
        s.out()
    );
    s.signal("TERM");
    assert!(s.wait_exit().success(), "{}", s.err());
}
