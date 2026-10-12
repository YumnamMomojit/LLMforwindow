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
    child: Option<OwnedChild>,
    state: Arc<Mutex<TunnelState>>,
    tail: Arc<Mutex<VecDeque<String>>>,
}

impl CloudflaredTunnel {
    pub fn new(exe: PathBuf, public_hostname: String) -> Self {
        Self {
            exe,
            public_hostname,
            child: None,
            state: Arc::new(Mutex::new(TunnelState::Off)),
            tail: Default::default(),
        }
    }

    pub fn state(&self) -> TunnelState {
        self.state.lock().unwrap().clone()
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
        if self.child.is_some() {
            return Err("tunnel already running".into());
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
            tokio::spawn(async move {
                let mut lines = pipe.lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let l = l.replace(&token, "[redacted]");
                    if l.contains("Registered tunnel connection") {
                        *state.lock().unwrap() = TunnelState::Connected;
                    }
                    let mut t = tail.lock().unwrap();
                    if t.len() >= 100 {
                        t.pop_front();
                    }
                    t.push_back(l);
                }
            });
        }
        self.child = Some(child);
        Ok(())
    }

    /// "Stop Sharing": ends only the tunnel; local engine and gateway keep running.
    pub async fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill_tree().await;
        }
        *self.state.lock().unwrap() = TunnelState::Off;
    }
}
