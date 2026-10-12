//! Public HTTPS ingress via a named Cloudflare Tunnel (`cloudflared`), kept replaceable.
//!
//! cloudflared makes an OUTBOUND connection; no router ports are opened. Cloudflare terminates
//! TLS and can see plaintext traffic. The tunnel's ingress rule (public hostname ->
//! http://127.0.0.1:<gateway port>) is configured in the Cloudflare dashboard (remotely-managed
//! tunnel); this module only runs the connector, with the token passed through the environment
//! (never argv, never logs). UNVERIFIED against a live Cloudflare account in this repository's
//! test environment; see docs/SHARING.md.
use crate::proc::OwnedChild;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum TunnelState {
    Off,
    Connecting,
    Connected,
    Failed(String),
}

/// Lines of the (redacted) cloudflared log carried in a `Failed` detail.
const FAILURE_TAIL_LINES: usize = 5;

/// Constructing this is the explicit authorization step to publish an endpoint.
pub struct PublicSharingConsent(());
impl PublicSharingConsent {
    pub fn granted_by_user() -> Self {
        PublicSharingConsent(())
    }
}

pub struct CloudflaredTunnel {
    pub exe: PathBuf,
    pub public_hostname: String,
    /// Behind a Mutex so `state(&self)` can notice (and reap) a cloudflared that exited.
    child: Mutex<Option<OwnedChild>>,
    state: Arc<Mutex<TunnelState>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    /// Bumped by every start and stop, so the log readers of an earlier connector cannot
    /// change the state of a later one (or turn Off back into Connected).
    run: Arc<AtomicU64>,
}

impl CloudflaredTunnel {
    pub fn new(exe: PathBuf, public_hostname: String) -> Self {
        Self {
            exe,
            public_hostname,
            child: Mutex::new(None),
            state: Arc::new(Mutex::new(TunnelState::Off)),
            tail: Default::default(),
            run: Default::default(),
        }
    }

    /// Current state. A connector that exited on its own (bad or revoked token, crash) is
    /// reported as Failed here, with its exit status and the last lines of its log.
    pub fn state(&self) -> TunnelState {
        let exited = {
            let mut child = self.child.lock().unwrap();
            let status = child.as_mut().and_then(|c| c.poll_exit());
            if status.is_some() {
                *child = None; // gone: start() may run again
            }
            status
        };
        if let Some(status) = exited {
            let tail = self.log_tail();
            let mut m = format!("cloudflared exited ({status})");
            let last = &tail[tail.len().saturating_sub(FAILURE_TAIL_LINES)..];
            if !last.is_empty() {
                m.push_str("\nLast cloudflared log lines (redacted):");
                for l in last {
                    m.push_str("\n  ");
                    m.push_str(l);
                }
            }
            *self.state.lock().unwrap() = TunnelState::Failed(m);
        }
        self.state.lock().unwrap().clone()
    }

    /// cloudflared's recent output with the token redacted, for diagnostics.
    pub fn log_tail(&self) -> Vec<String> {
        self.tail.lock().unwrap().iter().cloned().collect()
    }

    pub fn public_base_url(&self) -> Option<String> {
        (self.state() == TunnelState::Connected)
            .then(|| format!("https://{}/v1", self.public_hostname))
    }

    pub async fn start(
        &mut self,
        token: &str,
        _consent: &PublicSharingConsent,
    ) -> Result<(), String> {
        self.state(); // reaps a connector that already exited, so it does not block a restart
        if self.child.get_mut().unwrap().is_some() {
            return Err("tunnel already running".into());
        }
        if token.trim().is_empty() {
            return Err("the tunnel token is empty".into());
        }
        if !self.exe.is_file() {
            return Err(format!("cloudflared not found at {}", self.exe.display()));
        }
        let mut cmd = Command::new(&self.exe);
        cmd.args(["tunnel", "--no-autoupdate", "run"])
            .env("TUNNEL_TOKEN", token)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child =
            OwnedChild::spawn(&mut cmd).map_err(|e| format!("could not start cloudflared: {e}"))?;
        let run = self.run.fetch_add(1, Ordering::SeqCst) + 1;
        self.tail.lock().unwrap().clear();
        *self.state.lock().unwrap() = TunnelState::Connecting;
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
            let (state, tail, token) = (self.state.clone(), self.tail.clone(), token.to_string());
            let current = self.run.clone();
            tokio::spawn(async move {
                let mut lines = pipe.lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if current.load(Ordering::SeqCst) != run {
                        break; // stopped or restarted since
                    }
                    let l = l.replace(&token, "[redacted]");
                    if l.contains("Registered tunnel connection") {
                        let mut s = state.lock().unwrap();
                        if *s == TunnelState::Connecting {
                            *s = TunnelState::Connected;
                        }
                    }
                    let mut t = tail.lock().unwrap();
                    if t.len() >= 100 {
                        t.pop_front();
                    }
                    t.push_back(l);
                }
            });
        }
        *self.child.get_mut().unwrap() = Some(child);
        Ok(())
    }

    /// "Stop Sharing": ends only the tunnel; local engine and gateway keep running.
    pub async fn stop(&mut self) {
        self.run.fetch_add(1, Ordering::SeqCst);
        let child = self.child.get_mut().unwrap().take();
        if let Some(mut c) = child {
            c.kill_tree().await;
        }
        *self.state.lock().unwrap() = TunnelState::Off;
    }
}
