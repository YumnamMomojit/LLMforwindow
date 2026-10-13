//! packaging/hub.cmd on Windows: a `python` folder next to it is used only when it is a full
//! install. The embeddable package (a `python3XX._pth` beside python.exe) cannot run the engine,
//! so it is skipped and `hub.cmd check` says why. Run from a folder whose path has spaces, `&`
//! and `(x86)`. Windows only; checked under Wine's cmd (cargo test --target x86_64-pc-windows-gnu
//! with wine as the runner), not yet on a Windows host.
#![cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

fn check(root: &Path) -> String {
    let o = Command::new("cmd.exe")
        // /S: cmd strips exactly the outer quotes of what follows /C.
        .raw_arg(format!(
            "/D /S /C \"\"{}\" check\"",
            root.join("hub.cmd").display()
        ))
        .env_remove("HUB_PYTHON")
        .env_remove("HUB_COLIBRI_DIR")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).replace('\r', "")
}

/// The "Python   :" line of `hub.cmd check`.
fn python_line(out: &str) -> &str {
    out.lines()
        .find(|l| l.starts_with("  Python   :"))
        .unwrap_or_else(|| panic!("no Python line:\n{out}"))
}

#[test]
fn hub_cmd_skips_an_embeddable_python_folder() {
    let name = format!("packaging3 hubcmd & (x86) {}", std::process::id());
    let root = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("python")).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_hubd"), root.join("hubd.exe")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/hub.cmd"),
        root.join("hub.cmd"),
    )
    .unwrap();
    // Any executable stands in for python.exe: only which one hub.cmd picks matters here.
    std::fs::copy(
        env!("CARGO_BIN_EXE_hubd"),
        root.join("python").join("python.exe"),
    )
    .unwrap();
    // The folder name, not the whole path: %~dp0 may spell the temp folder differently (8.3).
    let local = format!("{name}\\python\\python.exe\"");

    let full = check(&root);
    assert!(
        python_line(&full).contains(&local),
        "a full install is used:\n{full}"
    );
    assert!(!full.contains("embeddable"), "{full}");

    std::fs::write(
        root.join("python").join("python312._pth"),
        "python312.zip\r\n.\r\n",
    )
    .unwrap();
    let embedded = check(&root);
    assert!(
        !python_line(&embedded).contains(&local),
        "the embeddable package was used:\n{embedded}"
    );
    assert!(
        embedded.contains("is NOT used: it is the embeddable Python")
            && embedded.contains("(python312._pth)"),
        "{embedded}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
