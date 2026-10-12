//! Inference adapter + Colibri implementation.
//!
//! `InferenceAdapter` is the narrow seam for other engines later. Generation and
//! cancellation travel over HTTP through the gateway (client disconnect => upstream cancel);
//! the adapter owns lifecycle, readiness and health. Nothing here substitutes another engine.
use crate::proc::OwnedChild;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
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
    child: Option<OwnedChild>,
    port: u16,
    log_tail: Arc<Mutex<VecDeque<String>>>,
}

fn actionable(tail: &str) -> String {
    let t = tail.to_ascii_lowercase();
    if t.contains("load_fail kind=nomem")
        || t.contains("memoryerror")
        || t.contains("cannot allocate")
    {
        "Not enough memory to load this model. Close other applications or choose a smaller model."
            .into()
    } else if t.contains("address already in use")
        || t.contains("winerror 10048")
        || t.contains("only one usage of each socket")
    {
        "The engine port is already in use. Pick another port or stop the program using it.".into()
    } else if t.contains("kind=format") || t.contains("kind=unsupported") {
        "The model files are not in a format this engine can serve. Re-validate the model folder."
            .into()
    } else if t.contains("kind=io")
        || t.contains("permission")
        || t.contains("no such file")
        || t.contains("winerror 5")
    {
        "The engine could not read the model files. Check that the folder exists and you can read it.".into()
    } else {
        "The engine exited during startup. See Diagnostics for the (redacted) engine log.".into()
    }
}

impl ColibriAdapter {
    pub fn new(cfg: ColibriConfig) -> Self {
        Self {
            cfg,
            state: Arc::new(Mutex::new(EngineState::NotConfigured)),
            child: None,
            port: 0,
            log_tail: Default::default(),
        }
    }

    pub fn upstream_url(&self) -> Option<String> {
        (self.port != 0).then(|| format!("http://127.0.0.1:{}", self.port))
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
        self.log_tail.lock().unwrap().clear();
        let script = self.cfg.colibri_dir.join("c").join("openai_server.py");
        if !self.cfg.python.is_file() {
            return Err(self.fail(format!("Python not found at {}", self.cfg.python.display())));
        }
        if !script.is_file() {
            return Err(self.fail(format!(
                "Colibri server script not found: {}",
                script.display()
            )));
        }
        if !self.cfg.model_dir.join("config.json").is_file() {
            return Err(self.fail(format!(
                "model folder has no config.json: {}",
                self.cfg.model_dir.display()
            )));
        }
        self.port = match self.free_port() {
            Ok(p) => p,
            Err(e) => return Err(self.fail(e)),
        };
        // Argument array, explicit paths, no shell. The internal credential goes in the
        // environment (argv is world-readable on many systems).
        let mut cmd = Command::new(&self.cfg.python);
        cmd.arg("-u")
            .arg(&script)
            .arg("--model")
            .arg(&self.cfg.model_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(self.port.to_string())
            .arg("--model-id")
            .arg(&self.cfg.model_id)
            .arg("--max-tokens")
            .arg(self.cfg.max_tokens.to_string());
        if let Some(e) = &self.cfg.engine_exe {
            cmd.arg("--engine").arg(e);
        }
        cmd.env("COLI_API_KEY", &self.cfg.internal_key)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env_remove("COLI_MODEL")
            .current_dir(&self.cfg.colibri_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = match OwnedChild::spawn(&mut cmd) {
            Ok(c) => c,
            Err(e) => return Err(self.fail(format!("could not start the engine process: {e}"))),
        };
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
            tokio::spawn(async move {
                let mut lines = pipe.lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let l = l.replace(&key, "[redacted]");
                    let mut t = tail.lock().unwrap();
                    if t.len() >= 200 {
                        t.pop_front();
                    }
                    t.push_back(l);
                }
            });
        }
        self.child = Some(child);
        // Readiness = the server answers /v1/models listing our model id. Never "process exists".
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}/v1/models", self.port);
        let t0 = Instant::now();
        loop {
            if let Some(c) = self.child.as_mut() {
                if let Ok(Some(status)) = c.child.try_wait() {
                    tokio::time::sleep(Duration::from_millis(200)).await; // let readers drain
                    let tail = self.log_tail().join("\n");
                    self.child = None;
                    return Err(self.fail(format!("{} (exit status {status})", actionable(&tail))));
                }
            }
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
                            self.set(EngineState::Ready);
                            return Ok(());
                        }
                    }
                }
            }
            if t0.elapsed() > self.cfg.startup_timeout {
                if let Some(c) = self.child.as_mut() {
                    c.kill_tree().await;
                }
                self.child = None;
                return Err(self.fail(format!("The engine did not become ready within {} s. Large models can take longer; raise the startup timeout.", self.cfg.startup_timeout.as_secs())));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn stop(&mut self) {
        self.set(EngineState::Stopping);
        if let Some(mut c) = self.child.take() {
            c.kill_tree().await;
        }
        self.port = 0;
        self.set(EngineState::NotConfigured);
    }

    fn state(&self) -> EngineState {
        // Reflect an unexpected exit after Ready.
        let cur = self.state.lock().unwrap().clone();
        if cur == EngineState::Ready {
            if let Some(c) = &self.child {
                if c.id().is_none() {
                    return EngineState::Failed("The engine process exited unexpectedly.".into());
                }
            }
        }
        cur
    }

    async fn health(&self) -> bool {
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
