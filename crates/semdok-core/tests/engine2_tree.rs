//! OwnedChild sees and ends the whole tree, not only its direct child: the engine is a
//! grandchild of semdok-edge (launcher -> engine). The Linux tests use the process group; the
//! Windows one (job accounting) passes under wine 9.0 but has not been run on a Windows host.
#![cfg(any(target_os = "linux", windows))]
use semdok_core::proc::OwnedChild;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Spawns `cmd` (stdout piped) and reads the first stdout line: a grandchild pid.
#[cfg(target_os = "linux")]
async fn spawn_with_grandchild(cmd: &mut Command) -> (OwnedChild, u32) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut c = OwnedChild::spawn(cmd).unwrap();
    let out = c.child.stdout.take().unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        BufReader::new(out).read_line(&mut line),
    )
    .await
    .expect("no grandchild pid")
    .unwrap();
    (c, line.trim().parse().unwrap())
}

#[cfg(target_os = "linux")]
fn kill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// The launcher stays, its engine dies: the count of live processes in the tree drops.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn tree_count_drops_when_a_grandchild_exits() {
    // The direct child must outlive its child, as upstream's launcher does: not `wait`, which
    // returns (and the shell exits) once the child is gone, but a process that never reaps.
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 600 & echo $!; exec sleep 600"]);
    let (mut c, engine) = spawn_with_grandchild(&mut cmd).await;
    assert_eq!(c.live_processes(), Some(2));
    kill(engine);
    let t0 = Instant::now();
    while c.live_processes() != Some(1) {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "count stayed {:?}",
            c.live_processes()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.kill_tree().await;
}

/// Same through the job: `cmd` stays, a short `ping` it started in the background ends.
#[cfg(windows)]
#[tokio::test]
async fn job_count_drops_when_a_grandchild_exits() {
    let mut cmd = Command::new("cmd");
    cmd.args([
        "/d",
        "/c",
        "start /b ping -n 4 127.0.0.1 >nul & ping -n 60 127.0.0.1 >nul",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let mut c = OwnedChild::spawn(&mut cmd).unwrap();
    // cmd and both pings, and a console host where Windows puts one in the job.
    let t0 = Instant::now();
    let mut peak = 0;
    while t0.elapsed() < Duration::from_millis(2000) {
        peak = peak.max(c.live_processes().expect("job not countable"));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(peak >= 3, "{peak}");
    // The short ping ends after about 3 s.
    let t0 = Instant::now();
    while c.live_processes() != Some(peak - 1) {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "count stayed {:?} (peak {peak})",
            c.live_processes()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.kill_tree().await;
    let t0 = Instant::now();
    while c.live_processes() != Some(0) {
        assert!(t0.elapsed() < Duration::from_secs(5), "job not emptied");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    // Gone, or a zombie nobody reaped yet: dead.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(s) => !matches!(
            s.rsplit_once(')')
                .and_then(|(_, r)| r.trim_start().chars().next()),
            Some('Z' | 'X')
        ),
    }
}

#[cfg(target_os = "linux")]
fn assert_dies(pid: u32) {
    let t0 = Instant::now();
    while alive(pid) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let still = alive(pid);
    if still {
        kill(pid);
    }
    assert!(!still, "grandchild {pid} survived");
}

/// The direct child exits on SIGTERM at once; a grandchild that ignores it must not outlive
/// kill_tree.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn kill_tree_ends_a_grandchild_that_ignores_sigterm() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "(trap '' TERM; exec sleep 600) & echo $!; wait"]);
    let (mut c, grandchild) = spawn_with_grandchild(&mut cmd).await;
    assert!(alive(grandchild));
    let t0 = Instant::now();
    c.kill_tree().await;
    assert!(
        t0.elapsed() < Duration::from_secs(4),
        "the direct child ignored SIGTERM"
    );
    assert_dies(grandchild);
}

/// Dropping the owner of an unreaped tree (a cancelled kill_tree, a replaced child) ends the
/// tree, not only the direct child (tokio's kill_on_drop).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_the_owner_ends_the_tree() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 600 & echo $!; wait"]);
    let (c, grandchild) = spawn_with_grandchild(&mut cmd).await;
    assert!(alive(grandchild));
    drop(c);
    assert_dies(grandchild);
}
