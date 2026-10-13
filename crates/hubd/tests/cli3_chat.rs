//! `hubd chat` startup and reply limits, through the real binary: Ctrl-C ends the chat while it
//! waits for a server that accepted the connection but never answers, and while the history
//! database is locked by another process; such a server is given up on after a bounded wait;
//! a reply cut at its token limit is marked, and so is a limit the server lowered. Unix only
//! (Ctrl-C is sent as SIGINT; the database lock is a POSIX record lock).
#![cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KEY: &str = "lah_0123abcd_testsecret";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cli3-chat-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let s = Scratch(p);
        let o = s.hubd(&["init"]).stdin(Stdio::null()).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        s
    }

    fn data(&self) -> PathBuf {
        self.0.join("data")
    }

    fn hubd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_hubd"));
        c.arg("--data-dir")
            .arg(self.data())
            .args(args)
            .env("RUST_BACKTRACE", "0")
            .env("HUB_API_KEY", KEY)
            .env_remove("HUB_DATA_DIR");
        c
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `hubd chat` whose stdin stays open (so it never ends for lack of input), output collected.
struct Running {
    child: Child,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
}

fn collect(mut r: impl Read + Send + 'static, buf: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut b = [0u8; 4096];
        while let Ok(n) = r.read(&mut b) {
            if n == 0 {
                break;
            }
            buf.lock().unwrap().extend_from_slice(&b[..n]);
        }
    });
}

impl Running {
    fn start(mut c: Command) -> Running {
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (out, err) = (Arc::default(), Arc::default());
        collect(child.stdout.take().unwrap(), Arc::clone(&out));
        collect(child.stderr.take().unwrap(), Arc::clone(&err));
        Running { child, out, err }
    }

    fn out(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&self.err.lock().unwrap()).into()
    }

    fn interrupt(&self) {
        let ok = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Exit code, or a panic if hubd is still running after `limit`.
    fn exit_within(&mut self, limit: Duration) -> Option<i32> {
        let t0 = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(100)); // let the pipes drain
                return st.code();
            }
            if t0.elapsed() > limit {
                let _ = self.child.kill();
                panic!(
                    "hubd chat still running after {limit:?}\nstdout: {}\nstderr: {}",
                    self.out(),
                    self.err()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port that accepts connections and never answers on them (a suspended `hubd serve`
/// looks like this: the kernel still completes the handshake). Counts the connections.
fn silent_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let n = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&n);
    std::thread::spawn(move || {
        let mut held = vec![];
        for s in l.incoming().flatten() {
            held.push(s);
            seen.fetch_add(1, Ordering::SeqCst);
        }
    });
    (addr, n)
}

fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

#[test]
fn ctrl_c_ends_the_chat_while_the_model_lookup_waits() {
    let s = Scratch::new("models-ctrlc");
    let (addr, conns) = silent_server();
    let url = format!("http://{addr}/v1");
    let mut c = Running::start(s.hubd(&["chat", "--url", &url, "--no-history"]));
    assert!(
        wait_until(Duration::from_secs(10), || conns.load(Ordering::SeqCst) > 0),
        "hubd never asked for /models\nstderr: {}",
        c.err()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(c.running(), "{}", c.err());
    c.interrupt();
    assert_eq!(c.exit_within(Duration::from_secs(5)), Some(1));
    assert!(c.err().contains("cancelled"), "{}", c.err());
    assert!(!c.out().contains("chatting with"), "{}", c.out());
}

/// Without Ctrl-C the wait still ends: the lookup has a time limit (30 s).
#[test]
fn a_server_that_never_answers_is_given_up_on() {
    let s = Scratch::new("models-timeout");
    let (addr, _) = silent_server();
    let url = format!("http://{addr}/v1");
    let mut c = Running::start(s.hubd(&["chat", "--url", &url, "--no-history"]));
    let t0 = Instant::now();
    assert_eq!(c.exit_within(Duration::from_secs(60)), Some(1));
    assert!(
        t0.elapsed() >= Duration::from_secs(25),
        "{:?}",
        t0.elapsed()
    );
    assert!(
        c.err().contains("did not answer within 30 s"),
        "{}",
        c.err()
    );
}

/// Whether `pid` has a SIGINT handler installed (Linux: read from /proc; elsewhere assumed
/// after a pause).
fn catches_sigint(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("SigCgt:"))
                    .and_then(|m| u64::from_str_radix(m.trim(), 16).ok())
            })
            .is_some_and(|m| m & (1 << (libc::SIGINT - 1)) != 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        std::thread::sleep(Duration::from_millis(800));
        true
    }
}

/// Holds SQLite's PENDING byte of `db` with a POSIX write lock, as a writer about to commit
/// does: another process can then not even start reading it, and waits (busy timeout).
struct DbLock {
    _file: std::fs::File,
}

impl DbLock {
    fn take(db: &Path) -> DbLock {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(db)
            .unwrap();
        // SAFETY: fcntl(F_SETLK) on an fd we own, with a zeroed and filled-in flock.
        unsafe {
            let mut l: libc::flock = std::mem::zeroed();
            l.l_type = libc::F_WRLCK as _;
            l.l_whence = libc::SEEK_SET as _;
            l.l_start = 0x4000_0000; // PENDING_BYTE
            l.l_len = 1;
            assert_eq!(libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &l), 0);
        }
        DbLock { _file: f }
    }
}

#[test]
fn ctrl_c_ends_the_chat_while_the_history_database_is_locked() {
    let s = Scratch::new("db-ctrlc");
    let _lock = DbLock::take(&s.data().join("hub.db"));
    // --model: no request at all; the URL is never contacted before a message is sent.
    let mut c = Running::start(s.hubd(&["chat", "--model", "m", "--url", "http://127.0.0.1:9/v1"]));
    // Once Ctrl-C is hubd's to handle (before that it would simply end it), the next step is
    // the database, which waits up to 5 s for the lock; Ctrl-C comes well before that.
    assert!(
        wait_until(Duration::from_secs(10), || catches_sigint(c.child.id())),
        "{}",
        c.err()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(c.running(), "{}", c.err());
    let t0 = Instant::now();
    c.interrupt();
    assert_eq!(c.exit_within(Duration::from_secs(10)), Some(1));
    assert!(
        t0.elapsed() < Duration::from_millis(2500),
        "Ctrl-C was only acted on after {:?}",
        t0.elapsed()
    );
    assert!(c.err().contains("cancelled"), "{}", c.err());
    assert!(!c.out().contains("chatting with"), "{}", c.out());
}

/// A fake local API: `/models` lists one model, every chat request streams `reply(body)` and
/// is recorded.
struct Api {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Api {
    fn start(reply: fn(&serde_json::Value) -> String) -> Api {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let b = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let b = Arc::clone(&b);
                std::thread::spawn(move || answer(s, &b, reply));
            }
        });
        Api { addr, bodies }
    }

    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }
}

fn answer(
    s: TcpStream,
    bodies: &Mutex<Vec<serde_json::Value>>,
    reply: fn(&serde_json::Value) -> String,
) {
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut first = String::new();
    if r.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        if let Some((n, v)) = h.split_once(':') {
            if n.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).unwrap();
    let mut s = s;
    if first.starts_with("GET") {
        let b = r#"{"object":"list","data":[{"id":"fake-model"}]}"#;
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        );
        return;
    }
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let sse = reply(&body);
    bodies.lock().unwrap().push(body);
    let _ = write!(
        s,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
    );
}

fn event(v: serde_json::Value) -> String {
    format!("data: {v}\n\n")
}

/// A reply of "part of an answer" that ended at its limit: the usage event reports as many
/// tokens as were asked for, or 1024 (the server's own limit) when that is lower.
fn cut_reply(body: &serde_json::Value) -> String {
    let n = body["max_tokens"].as_i64().unwrap_or(256).min(1024);
    [
        event(serde_json::json!({"choices": [{"index": 0, "delta": {"content": "part of an answer"}, "finish_reason": null}], "usage": null})),
        event(serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "length"}], "usage": null})),
        event(serde_json::json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": n, "total_tokens": n + 3}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat()
}

fn whole_reply(_: &serde_json::Value) -> String {
    [
        event(serde_json::json!({"choices": [{"index": 0, "delta": {"content": "a whole answer"}, "finish_reason": null}]})),
        event(serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat()
}

fn chat(s: &Scratch, api: &Api, extra: &[&str], input: &str) -> Output {
    let mut child = s
        .hubd(&[&["chat", "--url", &api.url(), "--no-history"], extra].concat())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    o
}

const CUT: &str = "[reply stopped at the token limit; use --max-tokens N, up to the server limit]";

#[test]
fn a_reply_cut_at_the_token_limit_is_marked() {
    let s = Scratch::new("cut");
    let api = Api::start(cut_reply);
    let o = chat(&s, &api, &[], "hello\n");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("part of an answer\n"), "{out}");
    assert!(out.contains(CUT), "{out}");
    assert!(!out.contains("fewer than"), "{out}");
    // Asked for usage, and for no particular length.
    let bodies = api.bodies.lock().unwrap().clone();
    assert_eq!(
        bodies[0]["stream_options"]["include_usage"], true,
        "{bodies:?}"
    );
    assert!(bodies[0].get("max_tokens").is_none(), "{bodies:?}");

    // Within the server's limit: marked, but not blamed on the server.
    let o = chat(&s, &api, &["--max-tokens", "300"], "hello\n");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains(CUT), "{out}");
    assert!(!out.contains("fewer than"), "{out}");
}

#[test]
fn a_limit_the_server_lowered_is_named() {
    let s = Scratch::new("clamped");
    let api = Api::start(cut_reply);
    let o = chat(&s, &api, &["--max-tokens", "8000"], "hello\n");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains(CUT), "{out}");
    assert!(
        out.contains("[the reply stopped after 1024 of the 8000 tokens asked for: either the server's limit (`hubd serve --max-output-tokens N`) or the model's context window, which a long conversation fills (/new starts fresh)]"),
        "{out}"
    );
    assert_eq!(api.bodies.lock().unwrap()[0]["max_tokens"], 8000);
}

#[test]
fn a_whole_reply_gets_no_note() {
    let s = Scratch::new("whole");
    let api = Api::start(whole_reply);
    let o = chat(&s, &api, &["--max-tokens", "8000"], "hello\n");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("a whole answer"), "{out}");
    assert!(!out.contains("token limit"), "{out}");
    assert!(!out.contains("fewer than"), "{out}");
}
