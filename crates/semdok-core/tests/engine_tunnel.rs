//! CloudflaredTunnel state against fake `cloudflared` shell scripts. Unix only.
#![cfg(unix)]
use semdok_core::tunnel::{CloudflaredTunnel, PublicSharingConsent, TunnelState};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TOKEN: &str = "tunnel-token-abcdef0123456789";

/// Held while a fake is written and while a connector is started (fork + exec; spawn returns
/// once the child has exec'd). A child forked by a parallel test while a fake is open for
/// writing keeps a copy of that write handle until it execs, and executing the fake
/// meanwhile fails with ETXTBSY ("Text file busy"); writing elsewhere and renaming does not
/// help, the inode is the same.
static FORK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fake(dir: &Path, body: &str) -> PathBuf {
    let _fork = FORK.lock().await;
    let p = dir.join("cloudflared");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

async fn start(t: &mut CloudflaredTunnel, token: &str) -> Result<(), String> {
    let _fork = FORK.lock().await;
    t.start(token, &PublicSharingConsent::granted_by_user())
        .await
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
    )
    .await;
    let mut t = CloudflaredTunnel::new(exe, "hub.example.com".into());
    start(&mut t, TOKEN).await.unwrap();
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
    start(&mut t, TOKEN).await.unwrap();
    t.stop().await;
    assert_eq!(t.state(), TunnelState::Off);
}

#[tokio::test]
async fn connector_that_dies_after_connecting_is_failed() {
    let dir = tempfile::tempdir().unwrap();
    let exe = fake(
        dir.path(),
        "echo 'INF Registered tunnel connection connIndex=0' >&2\nsleep 1\necho 'ERR lost' >&2\nexit 2",
    )
    .await;
    let mut t = CloudflaredTunnel::new(exe, "hub.example.com".into());
    start(&mut t, TOKEN).await.unwrap();
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
    )
    .await;
    let mut t = CloudflaredTunnel::new(exe, "hub.example.com".into());
    assert!(start(&mut t, "  ").await.is_err());
    start(&mut t, TOKEN).await.unwrap();
    wait_for(&t, "connected", |s| *s == TunnelState::Connected).await;
    t.stop().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.state(), TunnelState::Off);
}
