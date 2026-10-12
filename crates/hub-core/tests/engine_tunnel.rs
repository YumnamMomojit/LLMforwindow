//! CloudflaredTunnel state against fake `cloudflared` shell scripts. Unix only.
#![cfg(unix)]
use hub_core::tunnel::{CloudflaredTunnel, PublicSharingConsent, TunnelState};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TOKEN: &str = "tunnel-token-abcdef0123456789";

fn fake(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("cloudflared");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

async fn wait_for(t: &CloudflaredTunnel, what: &str, f: impl Fn(&TunnelState) -> bool) {
    let t0 = Instant::now();
    loop {
        let s = t.state();
        if f(&s) {
            return;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "never {what}: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn connector_that_exits_at_once_is_failed_and_can_be_restarted() {
    let dir = tempfile::tempdir().unwrap();
    let exe = fake(
        dir.path(),
        "echo \"INF token $TUNNEL_TOKEN\" >&2\necho 'ERR Provided Tunnel token is not valid.' >&2\nexit 1",
    );
    let mut t = CloudflaredTunnel::new(exe, "hub.example.com".into());
    let consent = PublicSharingConsent::granted_by_user();
    t.start(TOKEN, &consent).await.unwrap();
    wait_for(&t, "failed", |s| matches!(s, TunnelState::Failed(_))).await;
    let TunnelState::Failed(m) = t.state() else {
        unreachable!()
    };
    assert!(m.contains("cloudflared exited"), "{m}");
    assert_eq!(t.public_base_url(), None);
    // The log tail is exposed (and redacted) once the readers have drained.
    let t0 = Instant::now();
    while !t.log_tail().iter().any(|l| l.contains("not valid")) {
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t.log_tail());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let tail = t.log_tail().join("\n");
    assert!(
        tail.contains("[redacted]") && !tail.contains(TOKEN),
        "{tail}"
    );
    // An exited connector does not block a new start.
    t.start(TOKEN, &consent).await.unwrap();
    t.stop().await;
    assert_eq!(t.state(), TunnelState::Off);
}

#[tokio::test]
async fn connector_that_dies_after_connecting_is_failed() {
    let dir = tempfile::tempdir().unwrap();
    let exe = fake(
        dir.path(),
        "echo 'INF Registered tunnel connection connIndex=0' >&2\nsleep 1\necho 'ERR lost' >&2\nexit 2",
    );
    let mut t = CloudflaredTunnel::new(exe, "hub.example.com".into());
    t.start(TOKEN, &PublicSharingConsent::granted_by_user())
        .await
        .unwrap();
    wait_for(&t, "connected", |s| *s == TunnelState::Connected).await;
    assert_eq!(
        t.public_base_url().as_deref(),
        Some("https://hub.example.com/v1")
    );
    wait_for(&t, "failed", |s| matches!(s, TunnelState::Failed(_))).await;
    assert_eq!(t.public_base_url(), None);
    t.stop().await;
}

#[tokio::test]
async fn stop_stays_off_and_empty_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let exe = fake(
        dir.path(),
        "while true; do echo 'INF Registered tunnel connection'; sleep 0.01; done",
    );
    let mut t = CloudflaredTunnel::new(exe, "hub.example.com".into());
    let consent = PublicSharingConsent::granted_by_user();
    assert!(t.start("  ", &consent).await.is_err());
    t.start(TOKEN, &consent).await.unwrap();
    wait_for(&t, "connected", |s| *s == TunnelState::Connected).await;
    t.stop().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.state(), TunnelState::Off);
}
