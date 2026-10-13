//! Finding Python and the Colibri engine, and `semdok-edge check`.
//!
//! Done here rather than in semdok.cmd: cmd's `for /f` decodes a program's output with the console
//! (OEM) code page, so a Python under C:\Users\José\... came back garbled. Python is run with
//! `-X utf8` and its output is read as UTF-8, whatever the code pages say.
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a Python reported about itself.
pub(crate) struct PyInfo {
    pub exe: PathBuf,
    pub version: String,
    pub new_enough: bool,
    /// `sys.path[0]` is the script's folder. False for the embeddable package (a `._pth` file),
    /// which cannot run openai_server.py: it imports the files next to it.
    pub own_folder_importable: bool,
}

impl PyInfo {
    pub fn usable(&self) -> bool {
        self.new_enough && self.own_folder_importable
    }
}

const PROBE: &str = "import sys\nprint(sys.executable)\nprint('.'.join(map(str, sys.version_info[:3])))\nprint(int(sys.version_info >= (3, 10)))\nprint(int(sys.path[:1] == ['']))";

/// Runs `cmd pre... -X utf8 -c PROBE`. None if it does not run or does not answer like Python
/// (the Microsoft Store stub prints a message on stderr and exits 9009).
pub(crate) fn probe(cmd: &Path, pre: &[&str]) -> Option<PyInfo> {
    let o = Command::new(cmd)
        .args(pre)
        .args(["-X", "utf8", "-c", PROBE])
        .env("PYTHONIOENCODING", "utf-8")
        // semdok-core removes it for the engine too; here it would only hide a usable Python.
        .env_remove("PYTHONSAFEPATH")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8(o.stdout).ok()?;
    let mut l = s.lines().map(str::trim);
    let exe = PathBuf::from(l.next()?);
    let version = l.next()?.to_string();
    let new_enough = l.next()? == "1";
    let own_folder_importable = l.next()? == "1";
    if exe.as_os_str().is_empty() || !exe.is_file() {
        return None;
    }
    Some(PyInfo {
        exe,
        version,
        new_enough,
        own_folder_importable,
    })
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// Where a Python may be, in order. On Windows a `python` folder next to semdok-edge.exe comes first,
/// then the py launcher (it picks the newest install, unlike a bare `python`).
fn candidates() -> Vec<(PathBuf, Vec<&'static str>)> {
    let mut v = vec![];
    if cfg!(windows) {
        if let Some(d) = exe_dir() {
            let local = d.join("python").join("python.exe");
            if local.is_file() {
                v.push((local, vec![]));
            }
        }
        v.push((PathBuf::from("py"), vec!["-3"]));
        v.push((PathBuf::from("python"), vec![]));
    } else {
        v.push((PathBuf::from("python3"), vec![]));
        v.push((PathBuf::from("python"), vec![]));
    }
    v
}

/// The first usable Python, and what was found but cannot be used.
pub(crate) fn find_python() -> (Option<PyInfo>, Vec<PyInfo>) {
    let mut rejected = vec![];
    for (c, pre) in candidates() {
        if let Some(p) = probe(&c, &pre) {
            if p.usable() {
                return (Some(p), rejected);
            }
            rejected.push(p);
        }
    }
    (None, rejected)
}

/// A Colibri next to semdok-edge: `<dir>/colibri` as the official release zip unpacks it (flat), as a
/// source checkout (`c/`), or one folder deeper when the zip brought its own top-level folder.
pub(crate) fn find_colibri() -> Option<PathBuf> {
    let base = exe_dir()?.join("colibri");
    if semdok_core::engine::server_script(&base).is_some() {
        return Some(base);
    }
    let mut inner: Vec<PathBuf> = std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && semdok_core::engine::server_script(p).is_some())
        .collect();
    inner.sort();
    inner.into_iter().next()
}

/// `semdok-edge check`: what was found, and what to do about what was not.
pub(crate) fn check(dir: &Path, python: Option<PathBuf>, colibri: Option<PathBuf>) -> Result<()> {
    let readme = if cfg!(windows) {
        "README-FIRST.txt"
    } else {
        "README.md"
    };
    let mut ok = true;
    println!("Semdok Edge setup check");
    if let Ok(me) = std::env::current_exe() {
        println!(
            "  semdok-edge     : {} (version {})",
            me.display(),
            env!("CARGO_PKG_VERSION")
        );
    }
    println!(
        "  data     : {}{}",
        dir.display(),
        if dir.join("hub.db").is_file() {
            ""
        } else {
            " (created by `init`)"
        }
    );
    match python {
        Some(p) => match probe(&p, &[]) {
            Some(i) if i.usable() => {
                println!("  Python   : {} (version {})", i.exe.display(), i.version)
            }
            Some(i) if !i.new_enough => {
                ok = false;
                println!(
                    "  Python   : {} is version {}; 3.10 or newer is needed. {readme} step 1.",
                    p.display(),
                    i.version
                );
            }
            Some(_) => {
                ok = false;
                println!(
                    "  Python   : {} cannot run the engine: it does not import a script's own folder \
                     (the embeddable package, with a ._pth file). Install Python from python.org. {readme} step 1.",
                    p.display()
                );
            }
            None => {
                ok = false;
                println!(
                    "  Python   : {} does NOT run as Python. {readme} step 1.",
                    p.display()
                );
            }
        },
        None => {
            let (found, rejected) = find_python();
            for r in &rejected {
                let why = if !r.new_enough {
                    format!("version {} is older than 3.10", r.version)
                } else {
                    "the embeddable package (._pth file) cannot run the engine".into()
                };
                println!("  note     : {} is not used: {why}", r.exe.display());
            }
            match found {
                Some(i) => println!("  Python   : {} (version {})", i.exe.display(), i.version),
                None => {
                    ok = false;
                    println!(
                        "  Python   : NOT FOUND. Install Python 3.10 or newer from https://www.python.org/downloads/ \
                         (tick \"Add python.exe to PATH\"), then run check again. {readme} step 1."
                    );
                }
            }
        }
    }
    match colibri.or_else(find_colibri) {
        Some(c) => match semdok_core::engine::server_script(&c) {
            Some(_) => println!("  Colibri  : {}", c.display()),
            None => {
                ok = false;
                println!(
                    "  Colibri  : {} has no openai_server.py. {readme} step 2.",
                    c.display()
                );
            }
        },
        None => {
            ok = false;
            let into = exe_dir()
                .map(|d| d.join("colibri").display().to_string())
                .unwrap_or_else(|| "colibri".into());
            println!(
                "  Colibri  : NOT FOUND. Unzip the official Colibri v2.0.0 release into \"{into}\" so that \
                 openai_server.py is inside it, or pass --colibri-dir. {readme} step 2."
            );
        }
    }
    if !ok {
        bail!("setup is not complete (see above)");
    }
    println!("Ready. Next: init, then add a model ({readme} step 3).");
    Ok(())
}
