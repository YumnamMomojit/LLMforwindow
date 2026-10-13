//! Guards on the shipped packaging files (round 3): the launcher, the end-user guide and the bundle
//! script. hub.cmd itself is exercised under Wine's cmd by hand (see docs/WINDOWS.md); these tests
//! keep the fixes from being undone by an edit.

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Every text file a user or developer reads about packaging.
fn docs() -> Vec<(String, String)> {
    let mut out = vec![("README.md".to_string(), read("README.md"))];
    for dir in ["docs", "packaging"] {
        for e in std::fs::read_dir(repo().join(dir)).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() {
                let rel = format!("{dir}/{}", p.file_name().unwrap().to_string_lossy());
                out.push((rel.clone(), read(&rel)));
            }
        }
    }
    out
}

#[test]
fn the_untested_portable_script_is_gone_and_nothing_points_at_it() {
    assert!(!repo().join("packaging/build-portable.ps1").exists());
    for (name, text) in docs() {
        assert!(
            !text.contains("build-portable"),
            "{name} still mentions build-portable"
        );
    }
}

#[test]
fn hub_cmd_keeps_bangs_and_skips_an_embeddable_python() {
    let cmd = read("packaging/hub.cmd");
    assert!(
        cmd.contains("setlocal EnableExtensions DisableDelayedExpansion\r\n"),
        "delayed expansion would eat every ! (and cmd wants CRLF)"
    );
    // A python folder with a ._pth file beside python.exe (the embeddable package) is not used.
    let skip = cmd
        .find("for %%F in (*._pth) do set \"HUB_SKIPPED_PTH=%%F\"")
        .expect("._pth check");
    let pick = cmd
        .find("if not defined HUB_SKIPPED_PTH set \"HUB_PYTHON=%HUB_HOME%python\\python.exe\"")
        .expect("python folder pick");
    assert!(skip < pick);
    // check runs the Python it found and fails for one that drops the script's own folder.
    assert!(cmd.contains("sys.exit(sys.path[:1] != [''])"));
}

#[test]
fn readme_first_gives_only_advice_that_works() {
    let r = read("packaging/README-FIRST.txt");
    for gone in [
        "unzipped Python",
        "D:\\",
        "can also download colibri-v2.0.0-windows-x86_64-cuda.zip",
        "asks before downloading",
    ] {
        assert!(!r.contains(gone), "README-FIRST.txt still says {gone:?}");
    }
    for needed in [
        "pushd \"%USERPROFILE%\\Downloads\"",
        "--dir \"%USERPROFILE%\\colibri-models\"",
        "\"%USERPROFILE%\\colibri-models\\qwen3-coder-30b\"",
        "it does NOT ask before",
        "set COLI_VULKAN=1",
        "--max-output-tokens",
    ] {
        assert!(r.contains(needed), "README-FIRST.txt lacks {needed:?}");
    }
}

#[test]
fn make_bundle_ships_the_binary_cargo_just_built() {
    let s = read("packaging/make-bundle.sh");
    assert!(!s.contains("cp target/"), "copies from an assumed ./target");
    for needed in [
        "cargo metadata --format-version 1",
        "--message-format=json-render-diagnostics",
        "cargo clean --release -p hubd",
        "-nt \"$TMP/stamp\"",
        "--remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup",
        "--remap-path-prefix=$TARGET_DIR=/target",
        "-C strip=symbols",
    ] {
        assert!(s.contains(needed), "make-bundle.sh lacks {needed:?}");
    }
}
