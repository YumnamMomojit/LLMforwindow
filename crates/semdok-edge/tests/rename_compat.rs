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
    let old = home.join(".local/share/local-ai-hub");
    let new = home.join(".local/share/semdok-edge");
    // A data folder from before the rename, with its database.
    let o = run(&home, &[("SEMDOK_DATA_DIR", &old)], &["init"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = run(&home, &[], &["key", "create", "kept"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!new.exists(), "the old folder was not used");
    // An empty new folder (made by anything else) must not hide the old data.
    std::fs::create_dir_all(&new).unwrap();
    let o = run(&home, &[], &["key", "list"]);
    assert!(stdout(&o).contains("kept"), "{}", stdout(&o));
    // Once the new folder has its own database, it wins.
    let o = run(&home, &[("SEMDOK_DATA_DIR", &new)], &["init"]);
    assert!(o.status.success());
    let o = run(&home, &[], &["key", "list"]);
    assert!(!stdout(&o).contains("kept"), "{}", stdout(&o));
}

#[test]
fn an_empty_legacy_variable_is_named_in_the_error() {
    let home = scratch("empty-env");
    let o = Command::new(env!("CARGO_BIN_EXE_semdok-edge"))
        .args(["key", "list"])
        .env("HOME", &home)
        .env_remove("SEMDOK_DATA_DIR")
        .env("HUB_DATA_DIR", "")
        .env("RUST_BACKTRACE", "0")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("HUB_DATA_DIR is set but empty"), "{e}");
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
