//! `hubd check` and Python/Colibri discovery through the real binary, copied into a folder whose
//! path has non-ASCII characters, spaces, `&`, `(x86)` and `!`. A Python reached through a
//! non-ASCII folder must come back with its path intact (it is read as UTF-8 from `-X utf8`),
//! which is what hub.cmd's `for /f` garbled on Windows. Linux only (symlinks, /bin/sh fakes).
#![cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Bundle(PathBuf);

impl Bundle {
    fn new(tag: &str) -> Self {
        let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "gate setup {tag} bündle (x86) & !x {}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_hubd"), p.join("hubd")).unwrap();
        Bundle(p)
    }
    fn run(&self, args: &[&str], path: Option<&str>) -> Output {
        let mut c = Command::new(self.0.join("hubd"));
        c.arg("--data-dir").arg(self.0.join("data")).args(args);
        c.env_remove("HUB_PYTHON")
            .env_remove("HUB_COLIBRI_DIR")
            .env("RUST_BACKTRACE", "0");
        if let Some(p) = path {
            c.env("PATH", p);
        }
        c.stdin(Stdio::null()).output().unwrap()
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn python3() -> Option<PathBuf> {
    let o = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn check_finds_a_nested_colibri_and_a_python_under_a_non_ascii_folder() {
    let Some(py) = python3() else { return };
    let b = Bundle::new("found");
    std::fs::create_dir_all(b.0.join("colibri").join("colibri-v2.0.0-windows-x86_64")).unwrap();
    std::fs::write(
        b.0.join("colibri")
            .join("colibri-v2.0.0-windows-x86_64")
            .join("openai_server.py"),
        "",
    )
    .unwrap();
    let bin = b.0.join("Jösé Müller");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&py, bin.join("python3")).unwrap();
    let o = b.run(
        &["check"],
        Some(&format!("{}:/usr/bin:/bin", bin.display())),
    );
    let t = out(&o);
    assert!(o.status.success(), "{t}");
    assert!(t.contains("Jösé Müller/python3 (version 3."), "{t}");
    assert!(t.contains("colibri-v2.0.0-windows-x86_64"), "{t}");
    assert!(t.contains("Ready."), "{t}");
}

#[test]
fn check_explains_what_is_missing_and_fails() {
    let b = Bundle::new("missing");
    let o = b.run(&["check"], None);
    let t = out(&o);
    assert!(!o.status.success(), "{t}");
    assert!(
        t.contains("Colibri  : NOT FOUND. Unzip the official Colibri v2.0.0 release into"),
        "{t}"
    );
    let o = b.run(&["check", "--python", "/nonexistent/python"], None);
    assert!(out(&o).contains("does NOT run as Python"), "{}", out(&o));
}

#[test]
fn check_rejects_an_old_python_and_the_embeddable_package() {
    let b = Bundle::new("rejects");
    std::fs::create_dir_all(b.0.join("colibri")).unwrap();
    std::fs::write(b.0.join("colibri").join("openai_server.py"), "").unwrap();
    // Stand-ins answering the probe: executable, version, >= 3.10, own folder importable.
    let fake = |name: &str, lines: &str| {
        let p = b.0.join(name);
        std::fs::write(&p, format!("#!/bin/sh\nprintf '%s\\n{lines}' \"$0\"\n")).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p
    };
    let old = fake("old-python", "3.8.10\\n0\\n1\\n");
    let embed = fake("embed-python", "3.12.1\\n1\\n0\\n");
    let o = b.run(&["check", "--python", old.to_str().unwrap()], None);
    assert!(
        !o.status.success() && out(&o).contains("is version 3.8.10; 3.10 or newer is needed"),
        "{}",
        out(&o)
    );
    let o = b.run(&["check", "--python", embed.to_str().unwrap()], None);
    assert!(
        !o.status.success() && out(&o).contains("cannot run the engine"),
        "{}",
        out(&o)
    );
    // Discovery skips them and says why, then reports NOT FOUND when nothing else is on PATH.
    let bin = b.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::rename(&old, bin.join("python3")).unwrap();
    std::fs::rename(&embed, bin.join("python")).unwrap();
    let o = b.run(&["check"], Some(&format!("{}:/bin", bin.display())));
    let t = out(&o);
    assert!(!o.status.success(), "{t}");
    assert!(
        t.contains("is not used: version 3.8.10 is older than 3.10"),
        "{t}"
    );
    assert!(t.contains("is not used: the embeddable package"), "{t}");
    assert!(t.contains("Python   : NOT FOUND"), "{t}");
}

/// model validate and serve need no --colibri-dir when Colibri sits next to hubd.
#[test]
fn commands_use_the_colibri_next_to_hubd() {
    let b = Bundle::new("default-dir");
    let o = b.run(&["model", "validate", "/nonexistent-model"], None);
    assert!(
        out(&o).contains("Colibri not found: put it in a `colibri` folder next to hubd"),
        "{}",
        out(&o)
    );
    std::fs::create_dir_all(b.0.join("colibri")).unwrap();
    std::fs::write(b.0.join("colibri").join("openai_server.py"), "").unwrap();
    let o = b.run(&["model", "validate", "/nonexistent-model"], None);
    assert!(!out(&o).contains("Colibri not found"), "{}", out(&o));
}
