//! Linux: the tree count reads every /proc/<pid>/stat on the host, so one entry it cannot
//! read as text must not stop it. The kernel cuts a process name at 15 bytes, which can end
//! in the middle of a UTF-8 character ("ééééééééé" becomes "ééééééé\xc3"); a count that gave
//! up then left the engine-death check off for the whole session.
#![cfg(target_os = "linux")]
use semdok_core::proc::OwnedChild;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Gives the shell running `script` a name that is not UTF-8 in /proc (the lone first byte of
/// a two-byte character). The shell must stay itself afterwards: `exec` would rename it.
const RENAME: &str = r"printf '\303' > /proc/$$/comm || exit 1";

fn name_is_not_utf8(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/stat")).is_ok_and(|s| std::str::from_utf8(&s).is_err())
}

/// Spawns `sh -c script` (stdout piped) as an owned tree and reads the first stdout line.
async fn spawn_reading_a_line(script: &str) -> (OwnedChild, String) {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut c = OwnedChild::spawn(&mut cmd).unwrap();
    let out = c.child.stdout.take().unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        BufReader::new(out).read_line(&mut line),
    )
    .await
    .expect("no line from the child")
    .unwrap();
    (c, line.trim().to_string())
}

fn kill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

async fn wait_count(c: &OwnedChild, want: usize) {
    let t0 = Instant::now();
    while c.live_processes() != Some(want) {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "count stayed {:?}, want {want}",
            c.live_processes()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_non_utf8_name_outside_the_tree_does_not_stop_the_count() {
    // Another tree of ours, in its own process group, stands in for any process on the host.
    let (mut odd, ok) = spawn_reading_a_line(&format!("{RENAME}; echo ok; sleep 600; :")).await;
    assert_eq!(ok, "ok");
    assert!(name_is_not_utf8(odd.id().unwrap()), "setup: name is UTF-8");

    let (mut c, engine) = spawn_reading_a_line("sleep 600 & echo $!; exec sleep 600").await;
    let engine: u32 = engine.parse().unwrap();
    assert_eq!(c.live_processes(), Some(2));
    kill(engine);
    wait_count(&c, 1).await;
    c.kill_tree().await;
    odd.kill_tree().await;
}

#[tokio::test]
async fn a_tree_member_with_a_non_utf8_name_is_counted() {
    // The launcher itself carries the odd name; it waits for its engine, never reaping early.
    let (mut c, engine) =
        spawn_reading_a_line(&format!("{RENAME}; sleep 600 & echo $!; wait")).await;
    let engine: u32 = engine.parse().unwrap();
    let launcher = c.id().unwrap();
    assert!(name_is_not_utf8(launcher), "setup: name is UTF-8");
    assert_eq!(c.live_processes(), Some(2));
    assert_eq!(c.has_exited(), Some(false));
    kill(engine);
    // `wait` returns, the shell exits: a zombie until reaped, counted as gone at once.
    wait_count(&c, 0).await;
    assert_eq!(c.has_exited(), Some(true));
    let status = c.poll_exit().expect("exited launcher not reaped");
    assert!(status.success(), "{status}");
    assert_eq!(c.has_exited(), Some(true));
    c.kill_tree().await;
}
