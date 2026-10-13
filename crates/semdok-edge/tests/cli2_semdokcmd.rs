//! packaging/semdok.cmd on Windows: run from a folder whose path has spaces, `&`, `(x86)` and `!`,
//! with delayed expansion turned on (as the DelayedExpansion registry value does), every `!`
//! in the path and in the arguments still reaches semdok-edge.exe. Windows only; NOT run on a Windows
//! host yet (checked under Wine's cmd by hand).
#![cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

#[test]
fn semdok_cmd_keeps_bangs_with_delayed_expansion_on() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "cli2 semdokcmd & (x86) !bang {}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_semdok-edge"),
        root.join("semdok-edge.exe"),
    )
    .unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/semdok.cmd"),
        root.join("semdok.cmd"),
    )
    .unwrap();
    let data = root.join("data!");
    let run = |args: &str| {
        // /S: cmd strips exactly the outer quotes of what follows /C.
        let o = Command::new("cmd.exe")
            .raw_arg(format!(
                "/D /V:ON /S /C \"\"{}\" --data-dir \"{}\" {args}\"",
                root.join("semdok.cmd").display(),
                data.display()
            ))
            .env_remove("SEMDOK_DATA_DIR")
            .env_remove("HUB_DATA_DIR")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args}: {}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    run("init");
    run("workspace create \"Be brief!\"");
    let list = run("workspace list");
    assert!(list.contains("\tBe brief!\t"), "{list}");
    assert!(data.join("hub.db").exists());
    let _ = std::fs::remove_dir_all(&root);
}
