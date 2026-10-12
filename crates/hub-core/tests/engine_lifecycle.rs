//! ColibriAdapter supervision against a fake launcher: a small Python HTTP server standing in
//! for upstream `c/openai_server.py` (same argv, COLI_API_KEY, "listening on" line after the
//! bind, /v1/models). Unix only (python3 from PATH); skipped when python3 is missing.
#![cfg(unix)]
use hub_core::engine::{ColibriAdapter, ColibriConfig, EngineState, InferenceAdapter};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const KEY: &str = "internal-test-key-0123456789";

/// FAKE_MODE (read from <state>/mode): ok | die_after_ready | silent | busy_once.
const FAKE_LAUNCHER: &str = r#"
import argparse, json, os, sys, threading, time
from http.server import BaseHTTPRequestHandler, HTTPServer
p = argparse.ArgumentParser()
for f in ("--model", "--host", "--model-id", "--max-tokens", "--engine"):
    p.add_argument(f)
p.add_argument("--port", type=int)
a = p.parse_args()
state = os.environ["FAKE_STATE"]
mode = open(os.path.join(state, "mode")).read().strip()
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
        if mode == "die_after_ready":
            def die():
                time.sleep(0.5)
                print("MemoryError: the engine ran out of memory", file=sys.stderr, flush=True)
                time.sleep(0.5)
                os._exit(3)
            threading.Thread(target=die, daemon=True).start()

srv = HTTPServer((a.host, a.port), H)
print(f"loading with key {key}", file=sys.stderr, flush=True)
if mode != "silent":
    print(f"OpenAI-compatible API listening on http://{a.host}:{a.port}/v1", file=sys.stderr, flush=True)
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
    let t0 = Instant::now();
    let msg = loop {
        if let EngineState::Failed(m) = e.state() {
            break m;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "engine exit after Ready never reported"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(msg.contains("Not enough memory"), "{msg}");
    assert!(msg.contains("exit status"), "{msg}");
    assert!(msg.contains("[redacted]"), "log tail missing: {msg}");
    assert!(!msg.contains(KEY), "internal key leaked: {msg}");
    // No key goes to a port the engine no longer holds.
    assert_eq!(e.upstream_url(), None);
    assert!(!e.health().await);
    assert!(matches!(e.state(), EngineState::Failed(_)));
    e.stop().await;
    assert_eq!(e.state(), EngineState::NotConfigured);
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
