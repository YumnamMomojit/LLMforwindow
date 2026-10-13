//! Linux: an owned child dies with its parent even when the parent exits without any cleanup
//! (PR_SET_PDEATHSIG). The parent is this test binary re-run as `pdeathsig_parent`.
#![cfg(target_os = "linux")]
use semdok_core::proc::OwnedChild;
use std::time::{Duration, Instant};

const PARENT_ENV: &str = "SEMDOK_TEST_PDEATHSIG_PARENT";

/// Runs only as the re-executed parent: spawns a long `sleep` in its own process group,
/// reports its pid and exits at once, skipping every destructor (no kill_on_drop, no
/// kill_tree), like a semdok-edge that was SIGKILLed.
#[tokio::test]
async fn pdeathsig_parent() {
    if std::env::var_os(PARENT_ENV).is_none() {
        return;
    }
    let mut cmd = tokio::process::Command::new("sleep");
    // Not our stdout: the outer test reads it to EOF, which `sleep` would otherwise hold open.
    cmd.arg("60")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let c = OwnedChild::spawn(&mut cmd).unwrap();
    println!("CHILD_PID={}", c.id().unwrap());
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    std::process::exit(0);
}

fn alive(pid: u32) -> bool {
    // Gone, or a zombie nobody reaped yet (reparented to a pid 1 that may not reap): dead.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(s) => !matches!(
            s.rsplit_once(')')
                .and_then(|(_, r)| r.trim_start().chars().next()),
            Some('Z' | 'X')
        ),
    }
}

#[test]
fn child_dies_when_parent_dies_without_cleanup() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "pdeathsig_parent",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PARENT_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // libtest prints "test pdeathsig_parent ... " on the same line first.
    let pid: u32 = stdout
        .split_once("CHILD_PID=")
        .and_then(|(_, r)| r.split_whitespace().next())
        .unwrap_or_else(|| panic!("no pid from parent: {stdout}"))
        .parse()
        .unwrap();
    let t0 = Instant::now();
    while alive(pid) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let still = alive(pid);
    if still {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    assert!(!still, "child {pid} outlived its parent");
}
