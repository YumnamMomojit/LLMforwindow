//! `semdok-edge chat` against a small fake local API (std only): SSE framing edge cases (CRLF and CR
//! line ends, multi-line data, `data:` without a space), model output and server errors kept
//! inert in the terminal, Ctrl-C (stops a reply; at the prompt ends the chat), redirects never
//! followed, and history saved only for answered turns. Unix only (Ctrl-C is sent as SIGINT).
#![cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KEY: &str = "sem_0123abcd_testsecret";

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    auth: Option<String>,
    body: serde_json::Value,
}

enum Reply {
    Json(u16, String),
    /// Raw response head, e.g. a redirect.
    Head(String),
    /// SSE body pieces, each written after its delay; the connection then closes.
    Sse(Vec<(u64, Vec<u8>)>),
}

type Handler = dyn Fn(&Req, usize) -> Reply + Send + Sync;

/// A fake `semdok-edge serve` API on 127.0.0.1. The handler gets each request and the number of
/// earlier chat requests.
struct Api {
    addr: SocketAddr,
    reqs: Arc<Mutex<Vec<Req>>>,
}

impl Api {
    fn start(handler: impl Fn(&Req, usize) -> Reply + Send + Sync + 'static) -> Api {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let reqs: Arc<Mutex<Vec<Req>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let r = Arc::clone(&reqs);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let (r, h) = (Arc::clone(&r), Arc::clone(&handler));
                std::thread::spawn(move || serve_one(s, &r, &*h));
            }
        });
        Api { addr, reqs }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.addr.port())
    }

    fn requests(&self) -> Vec<Req> {
        self.reqs.lock().unwrap().clone()
    }

    fn chats(&self) -> Vec<Req> {
        self.requests()
            .into_iter()
            .filter(|r| r.path.ends_with("/chat/completions"))
            .collect()
    }
}

fn serve_one(s: TcpStream, reqs: &Mutex<Vec<Req>>, handler: &Handler) {
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
    );
    let (mut len, mut auth) = (0usize, None);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        let (name, value) = h.split_once(':').unwrap_or((&h, ""));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => len = value.trim().parse().unwrap_or(0),
            "authorization" => auth = Some(value.trim().to_string()),
            _ => {}
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).unwrap();
    let req = Req {
        method,
        path,
        auth,
        body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    };
    let earlier = {
        let mut all = reqs.lock().unwrap();
        let n = all
            .iter()
            .filter(|r| r.path.ends_with("/chat/completions"))
            .count();
        all.push(req.clone());
        n
    };
    let mut s = s;
    let _ = match handler(&req, earlier) {
        Reply::Json(code, b) => write!(
            s,
            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        ),
        Reply::Head(h) => s.write_all(h.as_bytes()),
        Reply::Sse(parts) => (|| {
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )?;
            for (delay, p) in parts {
                std::thread::sleep(Duration::from_millis(delay));
                s.write_all(&p)?;
                s.flush()?;
            }
            Ok(())
        })(),
    };
}

fn models() -> Reply {
    Reply::Json(
        200,
        r#"{"object":"list","data":[{"id":"fake-model"}]}"#.into(),
    )
}

/// One OpenAI-style content delta as a JSON string.
fn delta(field: &str, text: &str) -> String {
    serde_json::json!({"choices": [{"index": 0, "delta": {field: text}}]}).to_string()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cli2-chat-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let s = Scratch(p);
        let o = s.semdok(&["init"]).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        s
    }

    fn semdok(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_semdok-edge"));
        c.arg("--data-dir")
            .arg(self.0.join("data"))
            .args(args)
            .env("RUST_BACKTRACE", "0")
            .env("SEMDOK_API_KEY", KEY)
            .env_remove("SEMDOK_DATA_DIR");
        c
    }

    fn history(&self, args: &[&str]) -> String {
        let o = self
            .semdok(&[&["history"], args].concat())
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into()
    }

    /// (id, message count) of every saved conversation.
    fn conversations(&self) -> Vec<(String, usize)> {
        self.history(&["list"])
            .lines()
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let n = f[2].trim_end_matches(" messages").parse().unwrap();
                (f[0].to_string(), n)
            })
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running `semdok-edge chat` with stdin open and its output collected.
struct Chat {
    child: Child,
    stdin: Option<ChildStdin>,
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

impl Chat {
    fn start(s: &Scratch, api: &Api, extra: &[&str]) -> Chat {
        let mut child = s
            .semdok(&[&["chat", "--url", &api.url()], extra].concat())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (out, err) = (Arc::default(), Arc::default());
        collect(child.stdout.take().unwrap(), Arc::clone(&out));
        collect(child.stderr.take().unwrap(), Arc::clone(&err));
        Chat {
            stdin: child.stdin.take(),
            child,
            out,
            err,
        }
    }

    fn send(&mut self, line: &str) {
        let s = self.stdin.as_mut().unwrap();
        s.write_all(format!("{line}\n").as_bytes()).unwrap();
        s.flush().unwrap();
    }

    fn out(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&self.err.lock().unwrap()).into()
    }

    fn raw(&self) -> Vec<u8> {
        [&self.out.lock().unwrap()[..], &self.err.lock().unwrap()[..]].concat()
    }

    fn wait_for(&self, what: &str) {
        let t0 = Instant::now();
        while !self.out().contains(what) {
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "no '{what}' in\nstdout: {}\nstderr: {}",
                self.out(),
                self.err()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn interrupt(&self) {
        let ok = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    /// Exit code, or a panic if semdok-edge is still running after `secs`.
    fn exit_within(&mut self, secs: u64) -> Option<i32> {
        let t0 = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st.code();
            }
            if t0.elapsed() > Duration::from_secs(secs) {
                let _ = self.child.kill();
                panic!(
                    "semdok-edge chat still running after {secs} s\nstdout: {}\nstderr: {}",
                    self.out(),
                    self.err()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Sends the lines, closes stdin and waits for the exit code.
    fn finish(mut self, lines: &[&str]) -> (Option<i32>, Chat) {
        for l in lines {
            self.send(l);
        }
        self.stdin = None;
        let code = self.exit_within(30);
        // Let the collector threads drain the pipes.
        std::thread::sleep(Duration::from_millis(100));
        (code, self)
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn no_control_bytes(c: &Chat) {
    let raw = c.raw();
    for bad in [0x1b, 0x07, 0x9b] {
        assert!(!raw.contains(&bad), "byte {bad:#x} reached the terminal");
    }
    assert!(
        !String::from_utf8_lossy(&raw).contains('\u{9b}'),
        "C1 CSI reached the terminal"
    );
}

#[test]
fn crlf_cr_and_multi_line_events_are_understood_and_output_is_inert() {
    let s = Scratch::new("framing");
    let api = Api::start(|r, n| {
        if r.method == "GET" {
            return models();
        }
        let body = match n {
            // CRLF line ends, a CRLF split across writes, `data:` without a space, a comment,
            // a reasoning delta, and terminal control sequences in the model output.
            0 => vec![
                (0, b": keepalive\r\n\r\n".to_vec()),
                (
                    0,
                    format!("data: {}\r\n\r", delta("reasoning_content", "hmm\x1b[2J")).into(),
                ),
                (30, b"\n".to_vec()),
                (
                    0,
                    format!(
                        "data:{}\r\n\r\n",
                        delta("content", "\x1b]0;pwned\x07Hello\u{9b}31m")
                    )
                    .into(),
                ),
                (0, b"data: [DONE]\r\n\r\n".to_vec()),
            ],
            // One JSON event split over two data lines, CR-only line ends.
            _ => vec![
                (0, b"data: {\"choices\":[{\"index\":0,\r".to_vec()),
                (
                    0,
                    "data: \"delta\":{\"content\":\" world\"}}]}\r\rdata: [DONE]\r\r"
                        .as_bytes()
                        .to_vec(),
                ),
            ],
        };
        Reply::Sse(body)
    });
    let c = Chat::start(&s, &api, &[]);
    let (code, c) = c.finish(&["hi", "and more", "/exit"]);
    assert_eq!(code, Some(0), "stdout: {}\nstderr: {}", c.out(), c.err());
    let out = c.out();
    assert!(out.contains("chatting with fake-model"), "{out}");
    assert!(out.contains("]0;pwnedHello31m"), "{out}");
    assert!(out.contains(" world"), "{out}");
    assert!(!out.contains("stream ended"), "{out}");
    assert!(!out.contains("malformed"), "{out}");
    assert!(c.err().contains("hmm[2J"), "{}", c.err());
    no_control_bytes(&c);

    let chats = api.chats();
    assert_eq!(chats.len(), 2);
    for r in &chats {
        assert_eq!(r.auth.as_deref(), Some(format!("Bearer {KEY}").as_str()));
        assert_eq!(r.body["stream"], true);
        assert_eq!(r.body["model"], "fake-model");
    }
    // The second request carries the first answered turn.
    let m = &chats[1].body["messages"];
    assert_eq!(m[0]["content"], "hi");
    assert_eq!(m[1]["role"], "assistant");
    assert_eq!(m[2]["content"], "and more");

    let conv = s.conversations();
    assert_eq!(conv.len(), 1, "{conv:?}");
    assert_eq!(conv[0].1, 4); // two user and two assistant messages
    let shown = s.history(&["show", &conv[0].0]);
    assert!(shown.contains("[user]\nhi\n"), "{shown}");
    assert!(shown.contains(" world"), "{shown}");
    assert!(!shown.contains('\x1b'), "{shown:?}");
}

#[test]
fn server_errors_are_shown_inert_and_failed_turns_are_not_saved() {
    let s = Scratch::new("errors");
    let api = Api::start(|r, n| {
        if r.method == "GET" {
            return models();
        }
        match n {
            0 => Reply::Json(
                500,
                r#"{"error":{"message":"engine \u001b[31mexploded","code":"engine_error\u0007"}}"#
                    .into(),
            ),
            _ => Reply::Sse(vec![(
                0,
                b"data: {\"error\":{\"message\":\"cut \\u001b]0;x\\u0007off\",\"code\":\"stream_stalled\"}}\n\ndata: [DONE]\n\n"
                    .to_vec(),
            )]),
        }
    });
    let (code, c) = Chat::start(&s, &api, &[]).finish(&["one", "two", "/exit"]);
    assert_eq!(code, Some(0), "{}", c.err());
    let out = c.out();
    assert!(
        out.contains("[500 Internal Server Error: engine [31mexploded (engine_error)]"),
        "{out}"
    );
    assert!(out.contains("[cut ]0;xoff (stream_stalled)]"), "{out}");
    no_control_bytes(&c);
    // Neither turn was answered: nothing saved, not even an empty conversation.
    assert_eq!(s.conversations(), vec![]);
    // The failed turn is not resent as part of the conversation.
    let chats = api.chats();
    assert_eq!(chats[1].body["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn ctrl_c_at_the_prompt_ends_the_chat_even_after_a_reply() {
    let s = Scratch::new("ctrlc-prompt");
    let api = Api::start(|r, _| {
        if r.method == "GET" {
            return models();
        }
        Reply::Sse(vec![(
            0,
            format!("data: {}\n\ndata: [DONE]\n\n", delta("content", "pong")).into(),
        )])
    });
    let mut c = Chat::start(&s, &api, &[]);
    c.send("ping");
    c.wait_for("pong");
    c.wait_for("pong\n\n> ");
    std::thread::sleep(Duration::from_millis(200));
    c.interrupt();
    assert_eq!(c.exit_within(10), Some(0));
    assert_eq!(s.conversations().len(), 1);
}

#[test]
fn ctrl_c_during_a_reply_stops_it_and_the_chat_goes_on() {
    let s = Scratch::new("ctrlc-reply");
    let api = Api::start(|r, n| {
        if r.method == "GET" {
            return models();
        }
        match n {
            // Starts answering, then stalls far longer than the test.
            0 => Reply::Sse(vec![
                (
                    0,
                    format!("data: {}\n\n", delta("content", "partial")).into(),
                ),
                (60_000, b"data: [DONE]\n\n".to_vec()),
            ]),
            _ => Reply::Sse(vec![(
                0,
                format!("data: {}\n\ndata: [DONE]\n\n", delta("content", "complete")).into(),
            )]),
        }
    });
    let mut c = Chat::start(&s, &api, &[]);
    c.send("first");
    c.wait_for("partial");
    c.interrupt();
    c.wait_for("[stopped]");
    c.send("second");
    c.wait_for("complete");
    let (code, c) = c.finish(&["/exit"]);
    assert_eq!(code, Some(0), "{}", c.err());
    let chats = api.chats();
    assert_eq!(chats.len(), 2);
    let m = chats[1].body["messages"].as_array().unwrap().clone();
    assert_eq!(m.len(), 1, "the stopped turn was resent: {m:?}");
    assert_eq!(m[0]["content"], "second");
    let conv = s.conversations();
    assert_eq!(conv.len(), 1);
    assert_eq!(conv[0].1, 2, "only the answered turn is saved");
}

#[test]
fn redirects_are_never_followed() {
    let s = Scratch::new("redirect");
    let elsewhere = Api::start(|_, _| models());
    let target = format!("{}/models", elsewhere.url());
    let api = Api::start(move |_, _| {
        Reply::Head(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
    });
    let (code, c) = Chat::start(&s, &api, &[]).finish(&[]);
    assert_eq!(code, Some(1), "{}", c.out());
    assert!(c.err().contains("307"), "{}", c.err());
    assert!(elsewhere.requests().is_empty(), "the redirect was followed");
}

#[test]
fn no_history_saves_nothing_and_needs_no_database() {
    let s = Scratch::new("nohist");
    let api = Api::start(|r, _| {
        if r.method == "GET" {
            return models();
        }
        Reply::Sse(vec![(
            0,
            format!("data: {}\n\ndata: [DONE]\n\n", delta("content", "ok")).into(),
        )])
    });
    let (code, c) = Chat::start(&s, &api, &["--no-history"]).finish(&["hello"]);
    assert_eq!(code, Some(0), "{}", c.err());
    assert!(c.out().contains("History: not saved"), "{}", c.out());
    assert_eq!(s.conversations(), vec![]);

    // A data dir that does not exist is not created just to chat without history.
    let missing = s.0.join("never-created");
    let o = Command::new(env!("CARGO_BIN_EXE_semdok-edge"))
        .arg("--data-dir")
        .arg(&missing)
        .args(["chat", "--no-history", "--url", &api.url()])
        .env("SEMDOK_API_KEY", KEY)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!missing.exists());
}

#[test]
fn non_loopback_urls_and_bad_keys_are_refused_before_any_request() {
    let s = Scratch::new("refuse");
    for url in [
        "http://192.0.2.1:8787/v1",
        "https://127.0.0.1:8787/v1",
        "http://127.0.0.1:80@example.com/v1",
    ] {
        let o = s
            .semdok(&["chat", "--url", url])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(1), "{url}");
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("local API only"),
            "{url}"
        );
    }
    let api = Api::start(|_, _| models());
    for key in ["sk-not-ours", "sem_a b", "sem_x\u{7}"] {
        let o = s
            .semdok(&["chat", "--url", &api.url()])
            .env("SEMDOK_API_KEY", key)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(1), "{key:?}");
    }
    assert!(api.requests().is_empty(), "a bad key was sent");
}
