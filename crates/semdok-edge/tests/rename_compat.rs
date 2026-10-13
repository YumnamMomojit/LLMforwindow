//! The rename from "Local AI Hub" to Semdok Edge keeps existing setups working: the old `HUB_*`
//! environment variables are still read when the `SEMDOK_*` ones are unset, and an existing
//! `~/.local/share/local-ai-hub` data folder keeps being used when the new one does not exist.
//! (On Windows the same applies to `%LOCALAPPDATA%\LocalAIHub`.) Linux only (HOME layout).
#![cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn scratch(tag: &str) -> PathBuf {
    let p =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rename-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn run(home: &Path, env: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_semdok-edge"));
    c.args(args).env("HOME", home).env("RUST_BACKTRACE", "0");
    for v in [
        "SEMDOK_DATA_DIR",
        "HUB_DATA_DIR",
        "SEMDOK_PYTHON",
        "HUB_PYTHON",
        "SEMDOK_COLIBRI_DIR",
        "HUB_COLIBRI_DIR",
    ] {
        c.env_remove(v);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    c.stdin(Stdio::null()).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn a_fresh_install_uses_the_new_data_folder() {
    let home = scratch("fresh");
    let o = run(&home, &[], &["init"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(
        home.join(".local/share/semdok-edge/hub.db").is_file(),
        "{}",
        stdout(&o)
    );
    assert!(!home.join(".local/share/local-ai-hub").exists());
}

#[test]
fn an_existing_local_ai_hub_folder_keeps_being_used() {
    let home = scratch("legacy");
    std::fs::create_dir_all(home.join(".local/share/local-ai-hub")).unwrap();
    let o = run(&home, &[], &["init"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(
        home.join(".local/share/local-ai-hub/hub.db").is_file(),
        "{}",
        stdout(&o)
    );
    assert!(!home.join(".local/share/semdok-edge").exists());
    // Once the new folder exists, it wins.
    std::fs::create_dir_all(home.join(".local/share/semdok-edge")).unwrap();
    let o = run(&home, &[], &["init"]);
    assert!(o.status.success());
    assert!(home.join(".local/share/semdok-edge/hub.db").is_file());
}

#[test]
fn old_hub_variables_are_still_read_and_new_ones_win() {
    let home = scratch("env");
    let old = home.join("old-data");
    let new = home.join("new-data");
    let o = run(&home, &[("HUB_DATA_DIR", &old)], &["init"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(old.join("hub.db").is_file());
    let o = run(
        &home,
        &[("HUB_DATA_DIR", &old), ("SEMDOK_DATA_DIR", &new)],
        &["init"],
    );
    assert!(o.status.success());
    assert!(new.join("hub.db").is_file());
}
