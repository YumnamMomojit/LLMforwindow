//! Inference adapter + Colibri implementation.
//!
//! `InferenceAdapter` is the narrow seam for other engines later. Generation and
//! cancellation travel over HTTP through the gateway (client disconnect => upstream cancel);
//! the adapter owns lifecycle, readiness and health. Nothing here substitutes another engine.
use crate::proc::OwnedChild;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail")]
pub enum EngineState {
    NotConfigured,
    Starting,
    Ready,
    Stopping,
    Failed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub engine: &'static str,
    pub endpoints: Vec<&'static str>,
    pub max_concurrent_generations: usize,
    pub streaming: bool,
    pub cancellation: &'static str,
    pub tools: bool,
    pub multimodal_input: bool,
}

#[allow(async_fn_in_trait)]
pub trait InferenceAdapter {
    fn capabilities(&self) -> Capabilities;
    async fn start(&mut self) -> Result<(), String>;
    async fn stop(&mut self);
    fn state(&self) -> EngineState;
    /// Active health probe (does a model answer?), independent of process liveness.
    async fn health(&self) -> bool;
}

#[derive(Debug, Clone)]
pub struct ColibriConfig {
    pub python: PathBuf,
    /// Checkout / unpacked release containing `c/openai_server.py` (pinned in docs/UPSTREAM.md).
    pub colibri_dir: PathBuf,
    /// Optional explicit engine executable; otherwise the upstream launcher picks one.
    pub engine_exe: Option<PathBuf>,
    pub model_dir: PathBuf,
    pub model_id: String,
    /// 0 = pick a free loopback port.
    pub port: u16,
    pub internal_key: String,
    pub startup_timeout: Duration,
    pub max_tokens: u32,
}

pub struct ColibriAdapter {
    cfg: ColibriConfig,
    state: Arc<Mutex<EngineState>>,
    /// Behind a Mutex so `state(&self)` can reap an exit after Ready.
    child: Mutex<Option<OwnedChild>>,
    /// Port of a Ready engine, else 0. Only set once our own child serves it, and cleared as
    /// soon as the child is known to be gone, so the internal key is not sent to a port that
    /// something else may own.
    port: AtomicU16,
    log_tail: Arc<Mutex<VecDeque<String>>>,
}

/// Lines of the (redacted) engine log carried in a `Failed` detail.
const FAILURE_TAIL_LINES: usize = 10;

/// `t` is lowercase. WinError 10013 is also what a port reserved by Windows (Hyper-V/WinNAT
/// excluded ranges) or held exclusively by another account gives.
fn port_in_use(t: &str) -> bool {
    t.contains("address already in use")
        || t.contains("winerror 10048")
        || t.contains("only one usage of each socket")
        || t.contains("winerror 10013")
}

/// A specific, actionable reason found in the (redacted) engine log, if there is one.
fn actionable(tail: &str) -> Option<&'static str> {
    let t = tail.to_ascii_lowercase();
    if t.contains("load_fail kind=nomem")
        || t.contains("memoryerror")
        || t.contains("cannot allocate")
    {
        Some("Not enough memory to load this model. Close other applications or choose a smaller model.")
    } else if port_in_use(&t) {
        Some("The engine port is already in use. Pick another port or stop the program using it.")
    } else if t.contains("kind=format") || t.contains("kind=unsupported") {
        Some("The model files are not in a format this engine can serve. Re-validate the model folder.")
    } else if t.contains("can't open file") {
        // Python's own message for a missing script: the Colibri folder, not the model.
        Some("Python could not open the Colibri server script. Check the Colibri folder (--colibri-dir).")
    } else if t.contains("kind=io") || t.contains("permission") || t.contains("winerror 5") {
        Some("The engine could not read the model files. Check that the folder exists and you can read it.")
    } else if t.contains("no such file") {
        Some("The engine could not find a file it needs. Check the Colibri folder, the Python path and the model folder.")
    } else {
        None
    }
}

/// Absolute without touching the file system or adding a `\\?\` prefix (unlike canonicalize).
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Paths for one start, all absolute: the child runs with `current_dir = colibri_dir`, so a
/// relative path would be resolved a second time under it, and the pre-flight checks must
/// look at the same files the child will open.
struct LaunchPaths {
    python: PathBuf,
    script: PathBuf,
    colibri_dir: PathBuf,
    model_dir: PathBuf,
    engine_exe: Option<PathBuf>,
}

impl ColibriAdapter {
    pub fn new(cfg: ColibriConfig) -> Self {
        Self {
            cfg,
            state: Arc::new(Mutex::new(EngineState::NotConfigured)),
            child: Mutex::new(None),
            port: AtomicU16::new(0),
            log_tail: Default::default(),
        }
    }

    pub fn upstream_url(&self) -> Option<String> {
        let port = self.port.load(Ordering::SeqCst);
        (port != 0).then(|| format!("http://127.0.0.1:{port}"))
    }

    pub fn log_tail(&self) -> Vec<String> {
        self.log_tail.lock().unwrap().iter().cloned().collect()
    }

    fn set(&self, s: EngineState) {
        *self.state.lock().unwrap() = s;
    }

    fn fail(&self, msg: impl Into<String>) -> String {
        let m = msg.into();
        self.set(EngineState::Failed(m.clone()));
        m
    }

    /// Detail for an engine process that exited: the actionable reason (or `fallback`), the
    /// exit status and the last lines of the redacted log, so the operator sees why.
    fn exit_detail(&self, fallback: &str, status: ExitStatus) -> String {
        let tail = self.log_tail();
        let reason = actionable(&tail.join("\n")).unwrap_or(fallback);
        let mut m = format!("{reason} (exit status {status})");
        let last = &tail[tail.len().saturating_sub(FAILURE_TAIL_LINES)..];
        if !last.is_empty() {
            m.push_str("\nLast engine log lines (redacted):");
            for l in last {
                m.push_str("\n  ");
                m.push_str(l);
            }
        }
        m
    }

    fn free_port(&self) -> Result<u16, String> {
        let want = self.cfg.port;
        std::net::TcpListener::bind(("127.0.0.1", want))
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .map_err(|e| {
                if want == 0 {
                    format!("no free loopback port: {e}")
                } else {
                    format!("port {want} is not available: {e}")
                }
            })
    }
}

impl InferenceAdapter for ColibriAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            engine: "colibri",
            endpoints: vec!["GET /v1/models", "POST /v1/chat/completions"],
            max_concurrent_generations: 1,
            streaming: true,
            cancellation: "client disconnect closes the upstream connection; upstream sends CANCEL to the engine",
            tools: false,
            multimodal_input: false,
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        if matches!(self.state(), EngineState::Starting | EngineState::Ready) {
            return Err("engine already running".into());
        }
        self.set(EngineState::Starting);
        self.port.store(0, Ordering::SeqCst);
        let colibri_dir = absolute(&self.cfg.colibri_dir);
        let p = LaunchPaths {
            python: absolute(&self.cfg.python),
            script: colibri_dir.join("c").join("openai_server.py"),
            model_dir: absolute(&self.cfg.model_dir),
            engine_exe: self.cfg.engine_exe.as_deref().map(absolute),
            colibri_dir,
        };
        if !p.python.is_file() {
            return Err(self.fail(format!("Python not found at {}", p.python.display())));
        }
        if !p.script.is_file() {
            return Err(self.fail(format!(
                "Colibri server script not found: {}",
                p.script.display()
            )));
        }
        if !p.model_dir.join("config.json").is_file() {
            return Err(self.fail(format!(
                "model folder has no config.json: {}",
                p.model_dir.display()
            )));
        }
        // An auto-picked port is free when probed but not reserved, so something can take it
        // before the launcher binds. The launcher binds before loading the model and exits at
        // once with "address in use"; then try a fresh port.
        let attempts = if self.cfg.port == 0 { 3 } else { 1 };
        let mut attempt = 1;
        loop {
            match self.launch(&p).await {
                Ok(()) => return Ok(()),
                Err((_, true)) if attempt < attempts => attempt += 1,
                Err((m, _)) => return Err(self.fail(m)),
            }
        }
    }

    async fn stop(&mut self) {
        self.set(EngineState::Stopping);
        let child = self.child.get_mut().unwrap().take();
        if let Some(mut c) = child {
            c.kill_tree().await;
        }
        self.port.store(0, Ordering::SeqCst);
        self.set(EngineState::NotConfigured);
    }

    fn state(&self) -> EngineState {
        let cur = self.state.lock().unwrap().clone();
        if cur != EngineState::Ready {
            return cur;
        }
        // Reflect an unexpected exit after Ready. tokio's Child::id() stays Some until the
        // child is waited on, so only polling it (which also reaps) can tell.
        let exited = self
            .child
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|c| c.poll_exit());
        let Some(status) = exited else {
            return cur;
        };
        self.port.store(0, Ordering::SeqCst);
        let m = self.fail(self.exit_detail("The engine process exited unexpectedly.", status));
        EngineState::Failed(m)
    }

    async fn health(&self) -> bool {
        // state() first: it notices a dead engine and clears the port before any key is sent.
        if self.state() != EngineState::Ready {
            return false;
        }
        let Some(url) = self.upstream_url() else {
            return false;
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        matches!(http.get(format!("{url}/v1/models")).bearer_auth(&self.cfg.internal_key).send().await, Ok(r) if r.status().is_success())
    }
}

impl ColibriAdapter {
    /// One launch attempt. Err((message, port_in_use)); the caller decides on a retry and
    /// records the failure. On every failure path the port stays 0 and no child is kept.
    async fn launch(&mut self, p: &LaunchPaths) -> Result<(), (String, bool)> {
        let port = self.free_port().map_err(|e| (e, false))?;
        self.log_tail.lock().unwrap().clear();
        // Argument array, explicit absolute paths, no shell. The internal credential goes in
        // the environment (argv is world-readable on many systems). `--model-id=<id>` keeps an
        // id that starts with '-' from being read as an option.
        let mut cmd = Command::new(&p.python);
        cmd.arg("-u")
            .arg(&p.script)
            .arg("--model")
            .arg(&p.model_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg(format!("--model-id={}", self.cfg.model_id))
            .arg("--max-tokens")
            .arg(self.cfg.max_tokens.to_string());
        if let Some(e) = &p.engine_exe {
            cmd.arg("--engine").arg(e);
        }
        cmd.env("COLI_API_KEY", &self.cfg.internal_key)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env_remove("COLI_MODEL")
            .current_dir(&p.colibri_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = OwnedChild::spawn(&mut cmd)
            .map_err(|e| (format!("could not start the engine process: {e}"), false))?;
        // The launcher prints this after its own bind succeeded and the model is loaded
        // (openai_server.py serve()). Until then nothing is sent to the port, so a process that
        // grabbed it first never receives the internal key: our launcher fails to bind and
        // exits instead. Residual risk, accepted: if the engine dies later, the port can be
        // taken again; state() then reports Failed and clears it, but the gateway keeps its
        // own copy of the URL until hubd stops it.
        let listening = Arc::new(AtomicBool::new(false));
        let banner = format!("listening on http://127.0.0.1:{port}/");
        for pipe in [
            child.child.stdout.take().map(|s| {
                Box::new(BufReader::new(s)) as Box<dyn tokio::io::AsyncBufRead + Unpin + Send>
            }),
            child.child.stderr.take().map(|s| {
                Box::new(BufReader::new(s)) as Box<dyn tokio::io::AsyncBufRead + Unpin + Send>
            }),
        ]
        .into_iter()
        .flatten()
        {
            let tail = self.log_tail.clone();
            let key = self.cfg.internal_key.clone();
            let (listening, banner) = (listening.clone(), banner.clone());
            tokio::spawn(async move {
                let mut lines = pipe.lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let l = if key.is_empty() {
                        l
                    } else {
                        l.replace(&key, "[redacted]")
                    };
                    if l.contains(&banner) {
                        listening.store(true, Ordering::SeqCst);
                    }
                    let mut t = tail.lock().unwrap();
                    if t.len() >= 200 {
                        t.pop_front();
                    }
                    t.push_back(l);
                }
            });
        }
        *self.child.get_mut().unwrap() = Some(child);
        // Readiness = our launcher said it is listening AND the server answers /v1/models
        // listing our model id. Never "process exists".
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}/v1/models");
        let t0 = Instant::now();
        loop {
            let exited = self
                .child
                .get_mut()
                .unwrap()
                .as_mut()
                .and_then(|c| c.poll_exit());
            if let Some(status) = exited {
                tokio::time::sleep(Duration::from_millis(200)).await; // let readers drain
                *self.child.get_mut().unwrap() = None;
                let in_use = port_in_use(&self.log_tail().join("\n").to_ascii_lowercase());
                let m = self.exit_detail("The engine exited during startup.", status);
                return Err((m, in_use));
            }
            if listening.load(Ordering::SeqCst) {
                if let Ok(r) = http
                    .get(&url)
                    .bearer_auth(&self.cfg.internal_key)
                    .send()
                    .await
                {
                    if r.status().is_success() {
                        if let Ok(v) = r.json::<serde_json::Value>().await {
                            if v["data"].as_array().is_some_and(|a| {
                                a.iter().any(|m| m["id"] == self.cfg.model_id.as_str())
                            }) {
                                self.port.store(port, Ordering::SeqCst);
                                self.set(EngineState::Ready);
                                return Ok(());
                            }
                        }
                    }
                }
            }
            if t0.elapsed() > self.cfg.startup_timeout {
                if let Some(mut c) = self.child.get_mut().unwrap().take() {
                    c.kill_tree().await;
                }
                return Err((format!("The engine did not become ready within {} s. Large models can take longer; raise the startup timeout.", self.cfg.startup_timeout.as_secs()), false));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_script_is_not_blamed_on_the_model_folder() {
        let m = actionable(
            "python3: can't open file '/x/c/openai_server.py': [Errno 2] No such file or directory",
        )
        .unwrap();
        assert!(m.contains("Colibri"), "{m}");
        assert!(!m.contains("model files"), "{m}");
    }

    #[test]
    fn port_in_use_messages_are_recognised() {
        for l in [
            "OSError: [Errno 98] Address already in use",
            "OSError: [WinError 10048] Only one usage of each socket address is normally permitted",
            "PermissionError: [WinError 10013] An attempt was made to access a socket in a way forbidden by its access permissions",
        ] {
            assert!(port_in_use(&l.to_ascii_lowercase()), "{l}");
        }
        assert!(actionable("argument --model-id: expected one argument").is_none());
    }
}
