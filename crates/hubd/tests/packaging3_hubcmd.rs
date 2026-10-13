//! packaging/hub.cmd on Windows is a thin launcher: `hub.cmd check` reaches `hubd check`, which
//! finds a Colibri unzipped with its own top-level folder next to hub.cmd, from a folder whose
//! path has spaces, `&`, `(x86)` and non-ASCII characters. Python discovery and the embeddable-
//! package check live in hubd (tests/gate_setup.rs). Windows only; run under Wine's cmd
//! (CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine, with LANG=C.UTF-8 so Wine can map the
//! non-ASCII folder name), not yet on a Windows host.
#![cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

#[test]
fn hub_cmd_check_runs_hubd_check_and_finds_a_nested_colibri() {
    let name = format!("packaging3 hubcmd & (x86) Jösé {}", std::process::id());
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(&name);
    let _ = std::fs::remove_dir_all(&root);
    let inner = root.join("colibri").join("colibri-v2.0.0-windows-x86_64");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("openai_server.py"), "").unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_hubd"), root.join("hubd.exe")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/hub.cmd"),
        root.join("hub.cmd"),
    )
    .unwrap();
    let o = Command::new("cmd.exe")
        // /S: cmd strips exactly the outer quotes of what follows /C.
        .raw_arg(format!(
            "/D /S /C \"\"{}\" check\"",
            root.join("hub.cmd").display()
        ))
        .env_remove("HUB_PYTHON")
        .env_remove("HUB_COLIBRI_DIR")
        .env("HUB_DATA_DIR", root.join("data"))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).replace('\r', "");
    assert!(out.starts_with("Local AI Hub setup check"), "{out}");
    let colibri = out
        .lines()
        .find(|l| l.starts_with("  Colibri  :"))
        .unwrap_or_else(|| panic!("no Colibri line:\n{out}"));
    assert!(colibri.contains("colibri-v2.0.0-windows-x86_64"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}
