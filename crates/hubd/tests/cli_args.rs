//! `hubd` argument handling, through the real binary: help never has side effects, missing
//! words are usage errors (not panics), unknown or extra arguments and flags without values
//! are rejected, and values are range-checked before anything is written.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cli-args-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
    fn data(&self) -> PathBuf {
        self.0.join("data")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn hubd(data: &Path, args: &[&str], stdin: &str) -> Out {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hubd"));
    c.arg("--data-dir")
        .arg(data)
        .args(args)
        .env("RUST_BACKTRACE", "0")
        .env_remove("HUB_DATA_DIR")
        .env_remove("HUB_PYTHON")
        .env_remove("HUB_COLIBRI_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into(),
        stderr: String::from_utf8_lossy(&o.stderr).into(),
    }
}

fn init(s: &Scratch) {
    let o = hubd(&s.data(), &["init"], "");
    assert_eq!(o.code, Some(0), "{}", o.stderr);
}

/// Exit code 1 with the given message and the usage text; never a panic (101).
fn assert_usage_error(o: &Out, msg: &str) {
    assert_eq!(o.code, Some(1), "stdout: {} stderr: {}", o.stdout, o.stderr);
    assert!(o.stderr.contains(msg), "expected '{msg}' in: {}", o.stderr);
    assert!(o.stderr.contains("USAGE:"), "{}", o.stderr);
    assert!(!o.stderr.contains("panicked"), "{}", o.stderr);
}

fn key_list(s: &Scratch) -> String {
    let o = hubd(&s.data(), &["key", "list"], "");
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    o.stdout
}

#[test]
fn help_anywhere_prints_usage_and_does_nothing_else() {
    let s = Scratch::new("help");
    for args in [
        &["--help"][..],
        &["help"],
        &["purge", "--help"],
        &["init", "-h"],
        &["key", "create", "ci", "--help"],
        &["serve", "--model", "m", "--help"],
        &["model", "remove", "-h"],
    ] {
        let o = hubd(&s.data(), args, "");
        assert_eq!(o.code, Some(0), "{args:?}: {}", o.stderr);
        assert!(o.stdout.contains("USAGE:"), "{args:?}");
        assert!(!s.data().exists(), "{args:?} touched the data dir");
    }
}

#[test]
fn missing_words_are_usage_errors_not_panics() {
    let s = Scratch::new("missing");
    init(&s);
    for (args, msg) in [
        (&["model", "remove"][..], "missing model id"),
        (&["key", "revoke"], "missing key id"),
        (&["key", "rotate"], "missing key id"),
        (&["key", "create"], "missing key name"),
        (&["key"], "missing key subcommand"),
        (
            &["model", "add", "--id", "m", "--colibri-dir", "c"],
            "missing model directory",
        ),
        (
            &["model", "add", "dir", "--colibri-dir", "c"],
            "--id ID is required",
        ),
        (&["model", "validate", "dir"], "--colibri-dir C is required"),
        (&["serve", "--colibri-dir", "c"], "--model ID is required"),
        (&["secret", "set"], "missing secret name"),
    ] {
        assert_usage_error(&hubd(&s.data(), args, ""), msg);
    }
}

#[test]
fn unknown_and_extra_arguments_are_rejected() {
    let s = Scratch::new("extra");
    init(&s);
    for (args, bad) in [
        (&["purge", "--dry-run"][..], "--dry-run"),
        (&["init", "extra"], "extra"),
        (&["key", "list", "x"], "x"),
        (&["model", "remove", "a", "b"], "b"),
        (&["key", "create", "a", "b"], "b"),
        (
            &["model", "validate", "d", "--colibri-dir", "c", "--id", "x"],
            "--id",
        ),
        (
            &[
                "serve",
                "--model",
                "m",
                "--colibri-dir",
                "c",
                "--max-activ",
                "4",
            ],
            "--max-activ",
        ),
        (
            &[
                "serve",
                "--model",
                "m",
                "--colibri-dir",
                "c",
                "--port",
                "1",
                "--port",
                "2",
            ],
            "--port",
        ),
    ] {
        let o = hubd(&s.data(), args, "");
        assert_usage_error(&o, &format!("unexpected argument '{bad}'"));
        assert!(!o.stdout.contains("removed"), "{args:?} ran: {}", o.stdout);
    }
    assert_usage_error(&hubd(&s.data(), &["frobnicate"], ""), "unknown command");
    assert_usage_error(
        &hubd(&s.data(), &["key", "drop"], ""),
        "unknown key subcommand",
    );
}

#[test]
fn flags_without_values_are_errors_not_defaults() {
    let s = Scratch::new("novalue");
    init(&s);
    for (args, flag) in [
        // `--expires-days $DAYS` with DAYS unset must not create a key that never expires.
        (
            &["key", "create", "ci", "--expires-days"][..],
            "--expires-days",
        ),
        (
            &["key", "create", "ci", "--expires-days", ""],
            "--expires-days",
        ),
        (
            &["key", "create", "--expires-days", "--x", "ci"],
            "--expires-days",
        ),
        (&["key", "rotate", "id", "--grace-hours"], "--grace-hours"),
        (&["serve", "--model"], "--model"),
        (&["model", "add", "d", "--id", "--colibri-dir", "c"], "--id"),
    ] {
        assert_usage_error(&hubd(&s.data(), args, ""), &format!("{flag} needs a value"));
    }
    assert_eq!(key_list(&s), "", "a key was created");

    // A trailing --data-dir must not fall back to the current directory.
    let o = Command::new(env!("CARGO_BIN_EXE_hubd"))
        .args(["init", "--data-dir"])
        .env_remove("HUB_DATA_DIR")
        .current_dir(&s.0)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(!s.0.join("hub.db").exists());
    let o = Command::new(env!("CARGO_BIN_EXE_hubd"))
        .arg("init")
        .env("HUB_DATA_DIR", "")
        .current_dir(&s.0)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("HUB_DATA_DIR is set but empty"));
    assert!(!s.0.join("hub.db").exists());
}

#[test]
fn expires_days_is_a_bounded_positive_whole_number() {
    let s = Scratch::new("expiry");
    init(&s);
    for bad in [
        "0",
        "-5",
        "3651",
        "99999999999999",
        "9223372036854775807",
        "1.5",
        "ninety",
    ] {
        let o = hubd(
            &s.data(),
            &["key", "create", "ci", "--expires-days", bad],
            "",
        );
        assert_usage_error(&o, "--expires-days must be a whole number from 1 to 3650");
    }
    assert_eq!(key_list(&s), "", "an invalid expiry created a key");

    let o = hubd(
        &s.data(),
        &["key", "create", "ci", "--expires-days", "90"],
        "",
    );
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert!(o.stdout.contains("expires: in 90 days"), "{}", o.stdout);
    let list = key_list(&s);
    let field = |name: &str| -> i64 {
        list.split('\t')
            .find_map(|f| f.trim().strip_prefix(name))
            .unwrap()
            .parse()
            .unwrap()
    };
    assert_eq!(field("expires=") - field("created="), 90 * 86_400);
}

#[test]
fn max_output_tokens_is_bounded_and_documented() {
    let s = Scratch::new("max-out");
    for bad in ["0", "-1", "32769", "4k", "1e3"] {
        let o = hubd(
            &s.data(),
            &[
                "serve",
                "--model",
                "m",
                "--colibri-dir",
                "c",
                "--max-output-tokens",
                bad,
            ],
            "",
        );
        assert_usage_error(
            &o,
            "--max-output-tokens must be a whole number from 1 to 32768",
        );
    }
    let o = hubd(
        &s.data(),
        &[
            "serve",
            "--model",
            "m",
            "--colibri-dir",
            "c",
            "--max-output-tokens",
        ],
        "",
    );
    assert_usage_error(&o, "--max-output-tokens needs a value");
    assert!(!s.data().exists(), "a usage error touched the data dir");
    // The help names the flag with the gateway's actual defaults.
    let d = hub_core::gateway::GatewayConfig::default();
    let help = hubd(&s.data(), &["--help"], "").stdout;
    assert!(help.contains("[--max-output-tokens N]"), "{help}");
    assert!(
        help.contains(&format!("(1..32768, default {})", d.max_output_tokens)),
        "{help}"
    );
    assert!(
        help.contains(&format!(
            "a request without max_tokens gets {}",
            d.default_output_tokens
        )),
        "{help}"
    );
    assert!(
        !help.contains("{MAX_OUT}") && !help.contains("{DEFAULT_OUT}"),
        "{help}"
    );
}

#[test]
fn grace_hours_is_bounded() {
    let s = Scratch::new("grace");
    init(&s);
    for bad in ["-5", "721", "x"] {
        let o = hubd(
            &s.data(),
            &["key", "rotate", "someid", "--grace-hours", bad],
            "",
        );
        assert_usage_error(&o, "--grace-hours must be a whole number from 0 to 720");
    }
}

#[test]
fn empty_tunnel_token_is_not_stored() {
    let s = Scratch::new("token");
    init(&s);
    for stdin in ["", "\n", "   \r\n"] {
        let o = hubd(&s.data(), &["secret", "set", "tunnel_token"], stdin);
        assert_eq!(o.code, Some(1), "{stdin:?}");
        assert!(o.stderr.contains("nothing stored"), "{}", o.stderr);
    }
    let stored: Vec<_> = std::fs::read_dir(s.data().join("secrets"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("tunnel_token"))
        .collect();
    assert!(stored.is_empty(), "{stored:?}");
    let o = hubd(&s.data(), &["secret", "set", "tunnel_token"], "tok\n");
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_usage_error(
        &hubd(&s.data(), &["secret", "set", "other"], "x\n"),
        "only `secret set tunnel_token`",
    );
}

#[test]
fn sharing_flags_go_together() {
    let s = Scratch::new("share");
    init(&s);
    let base = ["serve", "--model", "m", "--colibri-dir", "c"];
    for extra in [
        &["--hostname", "h"][..],
        &["--cloudflared", "x"],
        &["--i-authorize-public-sharing"],
    ] {
        let o = hubd(&s.data(), &[&base[..], extra].concat(), "");
        assert_eq!(o.code, Some(1));
        assert!(
            o.stderr.contains("only apply together with --share"),
            "{}",
            o.stderr
        );
    }
    let o = hubd(
        &s.data(),
        &[&base[..], &["--share", "--hostname", "h"]].concat(),
        "",
    );
    assert_eq!(o.code, Some(1));
    assert!(o.stderr.contains("--share needs"), "{}", o.stderr);
}
