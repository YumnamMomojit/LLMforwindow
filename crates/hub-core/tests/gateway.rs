//! Protocol tests for the restricted gateway.
//! The upstream here is a TEST DOUBLE (not an LLM); real-engine tests live in scripts/.
use axum::{
    body::Body,
    extract::State,
    http::HeaderMap,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use hub_core::gateway::{Gateway, GatewayConfig};
use hub_core::{db, keys};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering::SeqCst},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const INTERNAL: &str = "internal-test-secret";

#[derive(Default)]
struct Mock {
    last_body: Mutex<Option<Value>>,
    auth_seen: Mutex<Vec<String>>,
    dropped: AtomicUsize,
    started: AtomicUsize,
}

struct DropGuard(Arc<Mock>);
impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, SeqCst);
    }
}

async fn mock_models() -> Json<Value> {
    Json(json!({"object":"list","data":[{"id":"test-model"}]}))
}

async fn mock_chat(State(m): State<Arc<Mock>>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    m.auth_seen.lock().unwrap().push(
        h.get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
    );
    *m.last_body.lock().unwrap() = Some(b.clone());
    m.started.fetch_add(1, SeqCst);
    let marker = b["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    if marker.starts_with("badctx") {
        return Response::builder().status(400).header("content-type", "application/json")
            .body(Body::from(r#"{"error":{"message":"prompt too long /secret/path","code":"context_length_exceeded"}}"#)).unwrap();
    }
    if marker.starts_with("stuckerr") {
        // error status whose body never completes
        let s = futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
        return Response::builder()
            .status(500)
            .body(Body::from_stream(s))
            .unwrap();
    }
    if b["stream"] == true {
        let guard = DropGuard(m.clone());
        if marker.starts_with("wedge") {
            // 200 headers, then not a single body byte
            let s = futures_util::stream::unfold(guard, |g| async move {
                std::future::pending::<()>().await;
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::new()), g))
            });
            return Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(s))
                .unwrap();
        }
        if marker.starts_with("flood") {
            // endless large chunks, enough to fill every buffer towards a client that stops reading
            let chunk = axum::body::Bytes::from(vec![b'x'; 128 << 10]);
            let s =
                futures_util::stream::repeat_with(move || Ok::<_, std::io::Error>(chunk.clone()));
            return Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(s))
                .unwrap();
        }
        let delay_first = if marker.starts_with("late") { 2500 } else { 0 };
        let slow =
            marker.starts_with("slow") || marker.starts_with("late") || marker.starts_with("q");
        let step = match () {
            _ if marker.starts_with("hold") => 700,
            _ if marker.starts_with("drip") => 300,
            _ if slow => 250,
            _ => 0,
        };
        // "trunc" ends cleanly after two events without ever sending [DONE]
        let truncated = marker.starts_with("trunc");
        let count = match () {
            _ if truncated => 2,
            _ if marker.starts_with("drip") => 1000,
            _ => 5,
        };
        let s = futures_util::stream::unfold((0u32, guard, marker), move |(i, g, mk)| async move {
            if i == 0 && delay_first > 0 {
                tokio::time::sleep(Duration::from_millis(delay_first)).await;
            }
            if i >= count {
                return None;
            }
            if step > 0 {
                tokio::time::sleep(Duration::from_millis(step)).await;
            }
            let chunk = format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":format!("{mk}#{i} ")}}]})
            );
            let out = if i + 1 == count && !truncated {
                format!("{chunk}data: [DONE]\n\n")
            } else {
                chunk
            };
            Some((
                Ok::<_, std::io::Error>(axum::body::Bytes::from(out)),
                (i + 1, g, mk),
            ))
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(s))
            .unwrap();
    }
    if marker.starts_with("slow") {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    if marker.starts_with("badjson") {
        return Response::builder()
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .unwrap();
    }
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"choices":[{"message":{"role":"assistant","content":format!("echo:{marker}")}}],
               "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}})
            .to_string(),
        ))
        .unwrap()
}

struct Env {
    gw: Gateway,
    mock: Arc<Mock>,
    base: String,
    token: String,
    db: rusqlite::Connection,
    _dir: tempfile::TempDir,
}

async fn setup(tweak: impl FnOnce(&mut GatewayConfig)) -> Env {
    let mock = Arc::new(Mock::default());
    let app = Router::new()
        .route("/v1/models", get(mock_models))
        .route("/v1/chat/completions", post(mock_chat))
        .with_state(mock.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = GatewayConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: format!("http://{up}"),
        model_id: "test-model".into(),
        db_path: dir.path().join("hub.db"),
        rate_limit_per_min: 0,
        ..Default::default()
    };
    tweak(&mut cfg);
    let dbc = db::open(&cfg.db_path).unwrap();
    let (_, token) = keys::create(&dbc, "t", None).unwrap();
    let gw = Gateway::start(cfg, INTERNAL.into()).await.unwrap();
    for _ in 0..50 {
        if gw.snapshot().ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gw.snapshot().ready, "gateway never became ready");
    let base = format!("http://{}", gw.addr);
    Env {
        gw,
        mock,
        base,
        token,
        db: dbc,
        _dir: dir,
    }
}

fn chat(msg: &str, stream: bool) -> Value {
    json!({"model":"test-model","messages":[{"role":"user","content":msg}],"stream":stream})
}
fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn post_chat(e: &Env, body: &Value) -> reqwest::Response {
    client()
        .post(format!("{}/v1/chat/completions", e.base))
        .bearer_auth(&e.token)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// Polls `f` for up to `secs` seconds.
async fn eventually(mut f: impl FnMut() -> bool, secs: u64) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

/// (status, outcome) of the newest request_log row for streaming or non-streaming requests.
fn last_log(e: &Env, stream: bool) -> Option<(i64, String)> {
    e.db.query_row(
        "SELECT status,outcome FROM request_log WHERE stream=?1 ORDER BY id DESC LIMIT 1",
        [stream as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// Raw HTTP/1.1 request head for the chat endpoint (for clients reqwest cannot imitate).
fn raw_head(e: &Env, content_length: usize) -> String {
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {}\r\n\
         content-type: application/json\r\ncontent-length: {content_length}\r\n\r\n",
        e.token
    )
}

#[tokio::test]
async fn auth_matrix() {
    let e = setup(|_| {}).await;
    let c = client();
    let url = format!("{}/v1/chat/completions", e.base);
    assert_eq!(
        c.post(&url)
            .json(&chat("hi", false))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.post(&url)
            .bearer_auth("lah_deadbeef_nope")
            .json(&chat("hi", false))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // the internal engine credential must not work on the public gateway
    assert_eq!(
        c.post(&url)
            .bearer_auth(INTERNAL)
            .json(&chat("hi", false))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(post_chat(&e, &chat("hi", false)).await.status(), 200);
    // revoke -> immediate effect
    let id = e.token[4..12].to_string();
    keys::revoke(&e.db, &id).unwrap();
    let r = post_chat(&e, &chat("hi", false)).await;
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["code"],
        "revoked_api_key"
    );
    // expired
    // create() refuses a past expiry, so make a valid key and age it in the database.
    assert!(keys::create(&e.db, "past", Some(1)).is_err());
    let (rec, tok) = keys::create(&e.db, "old", Some(i64::MAX / 2)).unwrap();
    e.db.execute("UPDATE api_keys SET expires_at=1 WHERE id=?1", [&rec.id])
        .unwrap();
    assert_eq!(
        c.post(&url)
            .bearer_auth(tok)
            .json(&chat("hi", false))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn internal_credential_replaces_client_key_and_routes_are_allowlisted() {
    let e = setup(|_| {}).await;
    assert_eq!(post_chat(&e, &chat("hi", false)).await.status(), 200);
    let seen = e.mock.auth_seen.lock().unwrap().clone();
    assert_eq!(seen, vec![format!("Bearer {INTERNAL}")]);
    let c = client();
    for p in [
        "/",
        "/v1/completions",
        "/v1/embeddings",
        "/v1/models/test-model",
        "/admin",
        "/healthz",
        "/v1/systemone",
        "/../etc/passwd",
        "/v1/chat/completions/x",
    ] {
        let r = c
            .get(format!("{}{}", e.base, p))
            .bearer_auth(&e.token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "GET {p}");
    }
    assert_eq!(
        c.post(format!("{}/v1/models", e.base))
            .bearer_auth(&e.token)
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    assert_eq!(
        c.get(format!("{}/v1/chat/completions", e.base))
            .bearer_auth(&e.token)
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    let m: Value = c
        .get(format!("{}/v1/models", e.base))
        .bearer_auth(&e.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(m["data"].as_array().unwrap().len(), 1);
    assert_eq!(m["data"][0]["id"], "test-model");
}

#[tokio::test]
async fn request_sanitising() {
    let e = setup(|c| c.max_output_tokens = 100).await;
    for extra in [
        json!({"logprobs":true}),
        json!({"tools":[]}),
        json!({"x_colibri_ignore_leading_stop":true}),
        json!({"response_format":{"type":"json_object"}}),
        json!({"n":2}),
        json!({"temperature":9}),
        json!({"stop":["a","b","c","d","e"]}),
        json!({"stop":""}),
        json!({"stop":"x".repeat(65)}),
    ] {
        let mut b = chat("hi", false);
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(post_chat(&e, &b).await.status(), 400, "{extra}");
    }
    let mut b = chat("hi", false);
    b["model"] = json!("other");
    assert_eq!(post_chat(&e, &b).await.status(), 404);
    let img = json!({"model":"test-model","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]});
    assert_eq!(post_chat(&e, &img).await.status(), 400);
    let tool = json!({"model":"test-model","messages":[{"role":"tool","content":"x"}]});
    assert_eq!(post_chat(&e, &tool).await.status(), 400);
    // clamp + ignored fields dropped + text parts flattened
    let ok = json!({"model":"test-model","max_tokens":99999,"seed":5,"user":"u","temperature":0.5,
        "stop":"END",
        "messages":[{"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]});
    assert_eq!(post_chat(&e, &ok).await.status(), 200);
    let seen = e.mock.last_body.lock().unwrap().clone().unwrap();
    assert_eq!(seen["max_tokens"], 100);
    assert_eq!(seen["stop"], json!(["END"]));
    assert!(seen.get("seed").is_none() && seen.get("user").is_none());
    assert_eq!(seen["messages"][0]["content"], "ab");
    // body limit and content type
    let big =
        json!({"model":"test-model","messages":[{"role":"user","content":"x".repeat(2 << 20)}]});
    assert_eq!(post_chat(&e, &big).await.status(), 413);
    let r = client()
        .post(format!("{}/v1/chat/completions", e.base))
        .bearer_auth(&e.token)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
}

#[tokio::test]
async fn upstream_errors_are_mapped_not_leaked() {
    let e = setup(|_| {}).await;
    let r = post_chat(&e, &chat("badctx", false)).await;
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded");
    let r = post_chat(&e, &chat("badctx", true)).await;
    assert!(r.text().await.unwrap().contains("context_length_exceeded"));
}

#[tokio::test]
async fn usage_and_metadata_logging_without_prompts() {
    let e = setup(|_| {}).await;
    post_chat(&e, &chat("super secret prompt", false)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (st, pt, ct): (i64, Option<i64>, Option<i64>) = e.db.query_row(
        "SELECT status,prompt_tokens,completion_tokens FROM request_log ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((st, pt, ct), (200, Some(7), Some(3)));
    let dump: String =
        e.db.query_row(
            "SELECT group_concat(key_id||model||outcome) FROM request_log",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!dump.contains("secret"));
}

#[tokio::test]
async fn sse_is_incremental_and_terminated() {
    let e = setup(|_| {}).await;
    let r = post_chat(&e, &chat("slow", true)).await;
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    let t0 = Instant::now();
    let mut s = r.bytes_stream();
    let mut arrivals = vec![];
    let mut text = String::new();
    while let Some(c) = s.next().await {
        let c = c.unwrap();
        arrivals.push(t0.elapsed());
        text.push_str(&String::from_utf8_lossy(&c));
    }
    assert!(text.trim_end().ends_with("data: [DONE]"));
    assert!(
        arrivals.len() >= 4,
        "chunks arrived coalesced: {arrivals:?}"
    );
    assert!(
        arrivals.last().unwrap().as_millis() - arrivals[0].as_millis() > 700,
        "not incremental: {arrivals:?}"
    );
}

#[tokio::test]
async fn client_disconnect_cancels_upstream_and_frees_slot() {
    let e = setup(|_| {}).await;
    let r = post_chat(&e, &chat("slow", true)).await;
    let mut s = r.bytes_stream();
    s.next().await.unwrap().unwrap();
    assert_eq!(e.gw.snapshot().active, 1);
    drop(s);
    for _ in 0..30 {
        if e.mock.dropped.load(SeqCst) == 1 && e.gw.snapshot().active == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        e.mock.dropped.load(SeqCst),
        1,
        "upstream stream was not abandoned"
    );
    assert_eq!(e.gw.snapshot().active, 0);
    // slot is reusable
    assert_eq!(post_chat(&e, &chat("hi", false)).await.status(), 200);
    let outcome: String =
        e.db.query_row("SELECT outcome FROM request_log WHERE stream=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(outcome, "cancelled");
}

#[tokio::test]
async fn bounded_queue_serialises_without_crossover() {
    let e = setup(|c| {
        c.max_queue = 1;
        c.max_active = 1;
    })
    .await;
    let mk = |m: &'static str| {
        let c = client();
        let url = format!("{}/v1/chat/completions", e.base);
        let t = e.token.clone();
        tokio::spawn(async move {
            c.post(url)
                .bearer_auth(t)
                .json(&chat(m, true))
                .send()
                .await
                .unwrap()
        })
    };
    let a = mk("qA");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = mk("qB");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let c3 = mk("qC");
    let (a, b, c3) = (a.await.unwrap(), b.await.unwrap(), c3.await.unwrap());
    assert_eq!(c3.status(), 429, "third request should find the queue full");
    assert!(c3.headers().contains_key("retry-after"));
    let (ta, tb) = (a.text().await.unwrap(), b.text().await.unwrap());
    assert!(ta.contains("qA#0") && !ta.contains("qB"), "{ta}");
    assert!(tb.contains("qB#0") && !tb.contains("qA"), "{tb}");
    assert_eq!(e.mock.started.load(SeqCst), 2);
}

#[tokio::test]
async fn per_key_rate_limit() {
    let e = setup(|c| c.rate_limit_per_min = 2).await;
    assert_eq!(post_chat(&e, &chat("a", false)).await.status(), 200);
    assert_eq!(post_chat(&e, &chat("b", false)).await.status(), 200);
    let r = post_chat(&e, &chat("c", false)).await;
    assert_eq!(r.status(), 429);
    assert!(r.headers().contains_key("retry-after"));
    // another key is unaffected
    let (_, t2) = keys::create(&e.db, "other", None).unwrap();
    let r = client()
        .post(format!("{}/v1/chat/completions", e.base))
        .bearer_auth(t2)
        .json(&chat("d", false))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn keepalive_comments_precede_slow_first_token() {
    let e = setup(|c| c.keepalive_secs = 1).await;
    let r = post_chat(&e, &chat("late", true)).await;
    assert_eq!(r.status(), 200);
    let mut s = r.bytes_stream();
    let first = String::from_utf8_lossy(&s.next().await.unwrap().unwrap()).to_string();
    assert!(first.starts_with(": keepalive"), "{first}");
}

#[tokio::test]
async fn client_giving_up_in_the_queue_frees_its_place() {
    let e = setup(|c| {
        c.max_active = 1;
        c.max_queue = 1;
    })
    .await;
    let holder = post_chat(&e, &chat("hold", true)).await;
    assert!(eventually(|| e.gw.snapshot().active == 1, 2).await);
    let url = format!("{}/v1/chat/completions", e.base);
    // Two clients queue behind the holder and give up; with max_queue=1 the second one
    // would be rejected outright if the first had leaked its place.
    for _ in 0..2 {
        let r = client()
            .post(&url)
            .bearer_auth(&e.token)
            .json(&chat("hi", false))
            .timeout(Duration::from_millis(300))
            .send()
            .await;
        assert!(
            r.as_ref().is_err_and(|err| err.is_timeout()),
            "expected to wait in the queue and give up: {r:?}"
        );
        assert!(
            eventually(|| e.gw.snapshot().queued == 0, 2).await,
            "abandoned waiter still counted: {:?}",
            e.gw.snapshot()
        );
    }
    // the slot is still busy, so a patient request queues (no 429) and runs after the holder
    assert_eq!(e.gw.snapshot().active, 1);
    assert_eq!(post_chat(&e, &chat("after", false)).await.status(), 200);
    assert!(holder.text().await.unwrap().contains("hold#4"));
    assert_eq!(e.gw.snapshot().queued, 0);
}

#[tokio::test]
async fn truncated_upstream_stream_is_reported_not_passed_off_as_complete() {
    let e = setup(|_| {}).await;
    let text = post_chat(&e, &chat("trunc", true))
        .await
        .text()
        .await
        .unwrap();
    assert!(text.contains("trunc#1"), "{text}");
    assert!(text.contains("\"engine_error\""), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    assert!(
        eventually(|| last_log(&e, true).is_some(), 2).await,
        "no log row"
    );
    assert_eq!(last_log(&e, true).unwrap(), (200, "truncated".into()));
    // a complete stream is still "ok"
    let text = post_chat(&e, &chat("full", true))
        .await
        .text()
        .await
        .unwrap();
    assert!(!text.contains("error"), "{text}");
    assert!(eventually(|| last_log(&e, true).unwrap().1 == "ok", 2).await);
    // non-streaming: a 2xx with an unparseable body is an engine error, and logged as one
    let r = post_chat(&e, &chat("badjson", false)).await;
    assert_eq!(r.status(), 502);
    assert!(eventually(|| last_log(&e, false).is_some(), 2).await);
    assert_eq!(last_log(&e, false).unwrap(), (502, "engine_error".into()));
}

#[tokio::test]
async fn first_byte_deadline_is_not_reset_by_keepalives() {
    let e = setup(|c| {
        c.keepalive_secs = 1;
        c.first_byte_timeout_secs = 2;
        c.stream_idle_timeout_secs = 1;
    })
    .await;
    let t0 = Instant::now();
    let r = post_chat(&e, &chat("wedge", true)).await;
    let text = tokio::time::timeout(Duration::from_secs(10), r.text())
        .await
        .expect("stream never ended: keepalives kept restarting the first-byte timeout")
        .unwrap();
    assert!(text.starts_with(": keepalive"), "{text}");
    assert!(text.contains("deadline_exceeded"), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    assert!(eventually(|| e.gw.snapshot().active == 0, 2).await);
    // an error status whose body never completes is bounded too
    let r = post_chat(&e, &chat("stuckerr", true)).await;
    let text = tokio::time::timeout(Duration::from_secs(10), r.text())
        .await
        .expect("error body read never ended")
        .unwrap();
    assert!(text.contains("engine_error"), "{text}");
}

#[tokio::test]
async fn stream_deadline_applies_while_output_continues() {
    let e = setup(|c| c.request_deadline_secs = 1).await;
    let t0 = Instant::now();
    let r = post_chat(&e, &chat("drip", true)).await;
    let text = tokio::time::timeout(Duration::from_secs(10), r.text())
        .await
        .expect("deadline never fired")
        .unwrap();
    assert!(text.contains("drip#0"), "{text}");
    assert!(text.contains("deadline_exceeded"), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn client_that_stops_reading_releases_the_slot() {
    use tokio::io::AsyncWriteExt;
    let e = setup(|c| c.stream_idle_timeout_secs = 1).await;
    let body = chat("flood", true).to_string();
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let mut s = sock.connect(e.gw.addr).await.unwrap();
    s.write_all(format!("{}{body}", raw_head(&e, body.len())).as_bytes())
        .await
        .unwrap();
    // `s` stays open and is never read
    assert!(eventually(|| e.gw.snapshot().active == 1, 2).await);
    assert!(
        eventually(|| e.gw.snapshot().active == 0, 8).await,
        "a client that stopped reading kept the generation slot"
    );
    assert_eq!(post_chat(&e, &chat("hi", false)).await.status(), 200);
    assert!(eventually(|| last_log(&e, true).is_some(), 2).await);
    assert_eq!(last_log(&e, true).unwrap(), (200, "client_stalled".into()));
    drop(s);
}

#[tokio::test]
async fn trickled_request_body_times_out() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let e = setup(|c| c.body_read_timeout_secs = 1).await;
    let mut s = tokio::net::TcpStream::connect(e.gw.addr).await.unwrap();
    // headers promise 100 body bytes, only a few ever arrive
    s.write_all(format!("{}{{\"model\"", raw_head(&e, 100)).as_bytes())
        .await
        .unwrap();
    let t0 = Instant::now();
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .expect("no response while the body trickles")
            .unwrap();
        assert!(n > 0, "connection closed without a response");
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 408"), "{head}");
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn host_allowlist_and_unready_engine_and_bind_guard() {
    let e = setup(|c| c.allowed_hosts = vec!["api.example.com".into()]).await;
    let c = client();
    let r = c
        .get(format!("{}/v1/models", e.base))
        .bearer_auth(&e.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .get(format!("{}/v1/models", e.base))
        .bearer_auth(&e.token)
        .header("host", "api.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // non-loopback bind refused by default
    let cfg = GatewayConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        db_path: e._dir.path().join("x.db"),
        ..Default::default()
    };
    assert!(Gateway::start(cfg, "k".into()).await.is_err());
    // upstream must be loopback
    let cfg = GatewayConfig {
        upstream: "http://example.com:80".into(),
        db_path: e._dir.path().join("y.db"),
        ..Default::default()
    };
    assert!(Gateway::start(cfg, "k".into()).await.is_err());
    // engine not reachable -> 503, never 200
    let dir = tempfile::tempdir().unwrap();
    let cfg = GatewayConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: "http://127.0.0.1:1".into(),
        model_id: "test-model".into(),
        db_path: dir.path().join("z.db"),
        ..Default::default()
    };
    let dbc = db::open(&cfg.db_path).unwrap();
    let (_, tok) = keys::create(&dbc, "t", None).unwrap();
    let gw = Gateway::start(cfg, "k".into()).await.unwrap();
    let r = c
        .post(format!("http://{}/v1/chat/completions", gw.addr))
        .bearer_auth(tok)
        .json(&chat("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[test]
fn key_store_never_keeps_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let c = db::open(&dir.path().join("k.db")).unwrap();
    let (rec, tok) = keys::create(&c, "n", None).unwrap();
    let secret = &tok[13..];
    let all: String = c
        .query_row("SELECT id||name||verifier||scopes FROM api_keys", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(!all.contains(secret));
    assert!(keys::verify(&c, Some(&format!("Bearer {tok}")), "inference").is_ok());
    assert_eq!(
        keys::verify(&c, Some(&format!("Bearer {tok}")), "admin").unwrap_err(),
        keys::AuthError::Forbidden
    );
    let (new, ntok) = keys::rotate(&c, &rec.id, 3600).unwrap();
    assert_ne!(new.id, rec.id);
    assert!(
        keys::verify(&c, Some(&format!("Bearer {tok}")), "inference").is_ok(),
        "grace period"
    );
    assert!(keys::verify(&c, Some(&format!("Bearer {ntok}")), "inference").is_ok());
}
