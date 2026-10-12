//! Restricted inference gateway: the only listener a tunnel or remote client may reach.
//!
//! Not a proxy: it exposes exactly `GET /v1/models` and `POST /v1/chat/completions`,
//! re-serialises a validated subset of the request body (so upstream extension fields such
//! as `x_colibri_*` and `logprobs` are unreachable), and talks to the raw Colibri server on
//! loopback with a separate internal credential. Forwarded-for headers are never trusted.
use crate::keys::{self, AuthError, SCOPE_INFERENCE};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures_util::StreamExt;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    pub listen: SocketAddr,
    /// Binding anything but loopback needs this explicit opt-in (private-network use).
    pub allow_non_loopback: bool,
    /// Raw Colibri server, loopback only, e.g. http://127.0.0.1:18080
    pub upstream: String,
    pub model_id: String,
    pub db_path: PathBuf,
    pub max_body_bytes: usize,
    pub max_messages: usize,
    /// Simultaneous generations. Colibri serves one at a time; keep 1 unless verified.
    pub max_active: usize,
    /// Requests allowed to wait for a free slot.
    pub max_queue: usize,
    pub queue_timeout_secs: u64,
    /// Whole-request deadline (non-streaming) / total stream duration.
    pub request_deadline_secs: u64,
    /// Max silence between upstream stream bytes once generation started.
    pub stream_idle_timeout_secs: u64,
    /// Max wait for the upstream to answer (covers long prompt prefill).
    pub first_byte_timeout_secs: u64,
    /// SSE comment cadence while waiting for first byte (proxies with ~100 s limits).
    pub keepalive_secs: u64,
    pub max_output_tokens: u32,
    pub default_output_tokens: u32,
    pub rate_limit_per_min: u32,
    /// If non-empty, the Host header (without port) must match one of these.
    pub allowed_hosts: Vec<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".parse().unwrap(),
            allow_non_loopback: false,
            upstream: "http://127.0.0.1:18080".into(),
            model_id: "local-model".into(),
            db_path: PathBuf::from("hub.db"),
            max_body_bytes: 1 << 20,
            max_messages: 256,
            max_active: 1,
            max_queue: 4,
            queue_timeout_secs: 30,
            request_deadline_secs: 900,
            stream_idle_timeout_secs: 120,
            first_byte_timeout_secs: 600,
            keepalive_secs: 15,
            max_output_tokens: 1024,
            default_output_tokens: 256,
            rate_limit_per_min: 30,
            allowed_hosts: vec![],
        }
    }
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub kind: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl ApiError {
    fn new(
        status: u16,
        kind: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap(),
            code,
            kind,
            message: message.into(),
            retry_after: None,
        }
    }
    fn bad(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", code, message)
    }
    fn body(&self) -> Value {
        json!({"error": {"message": self.message, "type": self.kind, "code": self.code}})
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.status, axum::Json(self.body())).into_response();
        if let Some(s) = self.retry_after {
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(s));
        }
        r
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct Inner {
    pub cfg: GatewayConfig,
    internal_key: String,
    db: Mutex<Connection>,
    client: reqwest::Client,
    sem: Arc<Semaphore>,
    waiting: AtomicUsize,
    active: AtomicUsize,
    ready: AtomicBool,
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewaySnapshot {
    pub ready: bool,
    pub active: usize,
    pub queued: usize,
}

/// Handle to a running gateway. Dropping it does not stop the server; call `shutdown`.
pub struct Gateway {
    pub inner: Arc<Inner>,
    pub addr: SocketAddr,
    stop: tokio::sync::watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Gateway {
    pub async fn start(cfg: GatewayConfig, internal_key: String) -> anyhow::Result<Gateway> {
        if !cfg.listen.ip().is_loopback() && !cfg.allow_non_loopback {
            anyhow::bail!(
                "refusing to listen on {}: non-loopback binds need allow_non_loopback",
                cfg.listen
            );
        }
        let up: reqwest::Url = cfg.upstream.parse()?;
        let host_ok = match up.host_str() {
            Some(h) => {
                h == "localhost"
                    || h.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }
            None => false,
        };
        if !host_ok || up.scheme() != "http" {
            anyhow::bail!("upstream must be http on loopback, got {}", cfg.upstream);
        }
        let db = crate::db::open(&cfg.db_path)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let inner = Arc::new(Inner {
            sem: Arc::new(Semaphore::new(cfg.max_active.max(1))),
            cfg,
            internal_key,
            db: Mutex::new(db),
            client,
            waiting: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            buckets: Mutex::new(HashMap::new()),
        });
        let listener = tokio::net::TcpListener::bind(inner.cfg.listen).await?;
        let addr = listener.local_addr()?;
        let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let mut srx = stop_rx.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = srx.wait_for(|v| *v).await;
                })
                .await;
        });
        let probe_inner = inner.clone();
        let probe = tokio::spawn(async move { health_probe(probe_inner, &mut stop_rx).await });
        Ok(Gateway {
            inner,
            addr,
            stop,
            tasks: vec![server, probe],
        })
    }

    pub fn snapshot(&self) -> GatewaySnapshot {
        self.inner.snapshot()
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
    }
}

impl Inner {
    pub fn snapshot(&self) -> GatewaySnapshot {
        GatewaySnapshot {
            ready: self.ready.load(SeqCst),
            active: self.active.load(SeqCst),
            queued: self.waiting.load(SeqCst),
        }
    }

    fn upstream_get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!(
                "{}{}",
                self.cfg.upstream.trim_end_matches('/'),
                path
            ))
            .bearer_auth(&self.internal_key)
    }

    fn take_token(&self, key_id: &str) -> Result<(), u64> {
        let rate = self.cfg.rate_limit_per_min as f64;
        if rate <= 0.0 {
            return Ok(());
        }
        let mut m = self.buckets.lock().unwrap();
        let b = m.entry(key_id.to_string()).or_insert(Bucket {
            tokens: rate,
            last: Instant::now(),
        });
        let now = Instant::now();
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate / 60.0).min(rate);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) * 60.0 / rate).ceil().max(1.0) as u64)
        }
    }
}

async fn health_probe(inner: Arc<Inner>, stop: &mut tokio::sync::watch::Receiver<bool>) {
    let mut fails = 0u32;
    loop {
        // While a generation is in flight the engine is demonstrably alive.
        if inner.active.load(SeqCst) == 0 {
            let ok = match inner
                .upstream_get("/v1/models")
                .timeout(Duration::from_secs(3))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r
                    .json::<Value>()
                    .await
                    .map(|v| {
                        v["data"].as_array().is_some_and(|a| {
                            a.iter().any(|m| m["id"] == inner.cfg.model_id.as_str())
                        })
                    })
                    .unwrap_or(false),
                _ => false,
            };
            if ok {
                fails = 0;
                inner.ready.store(true, SeqCst);
            } else {
                fails += 1;
                if fails >= 3 || !inner.ready.load(SeqCst) {
                    inner.ready.store(false, SeqCst);
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = stop.wait_for(|v| *v) => return,
        }
    }
}

/// Metadata-only request log. Never stores prompts or completions.
struct ReqLog {
    inner: Arc<Inner>,
    key_id: Option<String>,
    started: Instant,
    stream: bool,
    queue_ms: Option<i64>,
    first_byte_ms: Option<i64>,
    status: u16,
    outcome: &'static str,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
}

impl ReqLog {
    fn new(inner: &Arc<Inner>, key_id: Option<String>, stream: bool) -> Self {
        Self {
            inner: inner.clone(),
            key_id,
            started: Instant::now(),
            stream,
            queue_ms: None,
            first_byte_ms: None,
            status: 0,
            outcome: "cancelled",
            prompt_tokens: None,
            completion_tokens: None,
        }
    }
    fn usage(&mut self, v: &Value) {
        self.prompt_tokens = v["usage"]["prompt_tokens"].as_i64();
        self.completion_tokens = v["usage"]["completion_tokens"].as_i64();
    }
}

impl Drop for ReqLog {
    fn drop(&mut self) {
        let total = self.started.elapsed().as_millis() as i64;
        if let Ok(db) = self.inner.db.lock() {
            let _ = db.execute(
                "INSERT INTO request_log(ts,key_id,model,status,outcome,stream,queue_ms,first_byte_ms,total_ms,prompt_tokens,completion_tokens)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                rusqlite::params![crate::now(), self.key_id, self.inner.cfg.model_id, self.status, self.outcome,
                    self.stream as i64, self.queue_ms, self.first_byte_ms, total, self.prompt_tokens, self.completion_tokens],
            );
        }
    }
}

struct ActiveGuard(Arc<Inner>);
impl ActiveGuard {
    fn new(i: &Arc<Inner>) -> Self {
        i.active.fetch_add(1, SeqCst);
        Self(i.clone())
    }
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, SeqCst);
    }
}

fn host_allowed(cfg: &GatewayConfig, headers: &HeaderMap) -> bool {
    if cfg.allowed_hosts.is_empty() {
        return true;
    }
    let h = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let host = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        h.split(':').next().unwrap_or("").to_string()
    };
    cfg.allowed_hosts
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&host))
}

async fn handle(State(inner): State<Arc<Inner>>, req: Request) -> Response {
    let rid = hex::encode(crate::random_bytes::<8>());
    let mut resp = match route(inner, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    resp.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&format!("req_{rid}")).unwrap(),
    );
    resp.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

async fn route(inner: Arc<Inner>, req: Request) -> Result<Response, ApiError> {
    if !host_allowed(&inner.cfg, req.headers()) {
        return Err(ApiError::new(
            403,
            "invalid_request_error",
            "host_not_allowed",
            "Host header not allowed.",
        ));
    }
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/v1/models") => models(&inner, req.headers()),
        (Method::POST, "/v1/chat/completions") => chat(inner, req).await,
        (_, "/v1/models") | (_, "/v1/chat/completions") => Err(ApiError::new(
            405,
            "invalid_request_error",
            "method_not_allowed",
            "Method not allowed.",
        )),
        // Everything else (admin, files, health detail, upstream extras) does not exist here.
        _ => Err(ApiError::new(
            404,
            "invalid_request_error",
            "not_found",
            "Not found.",
        )),
    }
}

fn authenticate(inner: &Inner, headers: &HeaderMap) -> Result<keys::KeyRecord, ApiError> {
    let h = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let db = inner.db.lock().unwrap();
    keys::verify(&db, h, SCOPE_INFERENCE).map_err(|e| match e {
        AuthError::Missing => ApiError::new(
            401,
            "authentication_error",
            "missing_api_key",
            "Provide 'Authorization: Bearer <key>'.",
        ),
        AuthError::Invalid => ApiError::new(
            401,
            "authentication_error",
            "invalid_api_key",
            "Invalid API key.",
        ),
        AuthError::Revoked => ApiError::new(
            401,
            "authentication_error",
            "revoked_api_key",
            "This API key has been revoked.",
        ),
        AuthError::Expired => ApiError::new(
            401,
            "authentication_error",
            "expired_api_key",
            "This API key has expired.",
        ),
        AuthError::Forbidden => ApiError::new(
            403,
            "permission_error",
            "insufficient_scope",
            "This key lacks the required scope.",
        ),
    })
}

fn models(inner: &Inner, headers: &HeaderMap) -> Result<Response, ApiError> {
    authenticate(inner, headers)?;
    if !inner.ready.load(SeqCst) {
        return Err(unavailable());
    }
    Ok(axum::Json(json!({"object":"list","data":[{"id":inner.cfg.model_id,"object":"model","created":0,"owned_by":"local-ai-hub"}]}))
        .into_response())
}

fn unavailable() -> ApiError {
    let mut e = ApiError::new(
        503,
        "server_error",
        "engine_unavailable",
        "The model engine is not ready. Try again shortly.",
    );
    e.retry_after = Some(10);
    e
}

/// Validate and re-serialise a chat request. Returns (upstream body, stream?, include_usage?).
pub fn sanitize_chat_request(v: Value, cfg: &GatewayConfig) -> Result<(Value, bool), ApiError> {
    let obj = v
        .as_object()
        .ok_or_else(|| ApiError::bad("invalid_json", "Request body must be a JSON object."))?;
    const IGNORED: &[&str] = &["user", "seed", "store", "metadata", "service_tier"];
    for k in obj.keys() {
        let known = [
            "model",
            "messages",
            "stream",
            "stream_options",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "stop",
            "n",
        ]
        .contains(&k.as_str());
        if !known && !IGNORED.contains(&k.as_str()) {
            return Err(ApiError::bad(
                "unsupported_parameter",
                format!("Parameter '{k}' is not supported by this endpoint."),
            ));
        }
    }
    match obj.get("model").and_then(Value::as_str) {
        None => return Err(ApiError::bad("missing_model", "'model' is required.")),
        Some(m) if m != cfg.model_id => {
            return Err(ApiError::new(
                404,
                "invalid_request_error",
                "model_not_found",
                format!("Model '{m}' is not served. Use GET /v1/models."),
            ))
        }
        _ => {}
    }
    let msgs = obj
        .get("messages")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.len() <= cfg.max_messages)
        .ok_or_else(|| {
            ApiError::bad(
                "invalid_messages",
                format!(
                    "'messages' must be an array of 1-{} items.",
                    cfg.max_messages
                ),
            )
        })?;
    let mut out_msgs = Vec::with_capacity(msgs.len());
    for m in msgs {
        let role = m["role"].as_str().unwrap_or("");
        if !["system", "user", "assistant"].contains(&role) {
            return Err(ApiError::bad(
                "unsupported_role",
                "Message roles supported: system, user, assistant.",
            ));
        }
        let content = match &m["content"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => {
                let mut s = String::new();
                for p in parts {
                    if p["type"] != "text" || !p["text"].is_string() {
                        return Err(ApiError::bad(
                            "unsupported_content",
                            "Only text content is supported.",
                        ));
                    }
                    s.push_str(p["text"].as_str().unwrap());
                }
                s
            }
            _ => {
                return Err(ApiError::bad(
                    "invalid_messages",
                    "Message 'content' must be a string or text parts.",
                ))
            }
        };
        out_msgs.push(json!({"role": role, "content": content}));
    }
    if let Some(n) = obj.get("n") {
        if n.as_i64() != Some(1) {
            return Err(ApiError::bad(
                "unsupported_parameter",
                "Only n=1 is supported.",
            ));
        }
    }
    let stream = match obj.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        _ => {
            return Err(ApiError::bad(
                "invalid_stream",
                "'stream' must be a boolean.",
            ))
        }
    };
    let mut out = json!({"model": cfg.model_id, "messages": out_msgs, "stream": stream});
    let mut max = cfg.default_output_tokens as i64;
    for k in ["max_tokens", "max_completion_tokens"] {
        match obj.get(k) {
            None | Some(Value::Null) => {}
            Some(x) => match x.as_i64() {
                Some(n) if n >= 1 => max = n,
                _ => {
                    return Err(ApiError::bad(
                        "invalid_max_tokens",
                        format!("'{k}' must be a positive integer."),
                    ))
                }
            },
        }
    }
    out["max_tokens"] = json!(max.min(cfg.max_output_tokens as i64));
    for (k, lo, hi) in [("temperature", 0.0, 2.0), ("top_p", 0.0, 1.0)] {
        if let Some(x) = obj.get(k).filter(|x| !x.is_null()) {
            match x.as_f64() {
                Some(f) if (lo..=hi).contains(&f) => out[k] = json!(f),
                _ => {
                    return Err(ApiError::bad(
                        "invalid_parameter",
                        format!("'{k}' must be a number in [{lo}, {hi}]."),
                    ))
                }
            }
        }
    }
    match obj.get("stop") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => out["stop"] = json!([s]),
        Some(Value::Array(a))
            if a.len() <= 4
                && a.iter()
                    .all(|s| s.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 64)) =>
        {
            out["stop"] = Value::Array(a.clone())
        }
        _ => {
            return Err(ApiError::bad(
                "invalid_stop",
                "'stop' must be up to 4 non-empty strings (max 64 bytes).",
            ))
        }
    }
    if let Some(so) = obj.get("stream_options").filter(|x| !x.is_null()) {
        match so.get("include_usage") {
            Some(Value::Bool(b)) if so.as_object().unwrap().len() == 1 => {
                out["stream_options"] = json!({"include_usage": b})
            }
            _ => {
                return Err(ApiError::bad(
                    "invalid_stream_options",
                    "Only stream_options.include_usage is supported.",
                ))
            }
        }
    }
    Ok((out, stream))
}

enum Admission {
    Granted(tokio::sync::OwnedSemaphorePermit, i64),
}

async fn admit(inner: &Arc<Inner>) -> Result<Admission, ApiError> {
    let t0 = Instant::now();
    if let Ok(p) = inner.sem.clone().try_acquire_owned() {
        return Ok(Admission::Granted(p, 0));
    }
    if inner.waiting.fetch_add(1, SeqCst) >= inner.cfg.max_queue {
        inner.waiting.fetch_sub(1, SeqCst);
        let mut e = ApiError::new(
            429,
            "rate_limit_error",
            "queue_full",
            "Too many requests are waiting for the model. Retry later.",
        );
        e.retry_after = Some(5);
        return Err(e);
    }
    let r = tokio::time::timeout(
        Duration::from_secs(inner.cfg.queue_timeout_secs),
        inner.sem.clone().acquire_owned(),
    )
    .await;
    inner.waiting.fetch_sub(1, SeqCst);
    match r {
        Ok(Ok(p)) => Ok(Admission::Granted(p, t0.elapsed().as_millis() as i64)),
        _ => {
            let mut e = ApiError::new(
                503,
                "server_error",
                "queue_timeout",
                "Timed out waiting for the model.",
            );
            e.retry_after = Some(10);
            Err(e)
        }
    }
}

fn upstream_error(status: u16, body: &[u8]) -> ApiError {
    if matches!(status, 400 | 404 | 413 | 422) {
        if let Ok(v) = serde_json::from_slice::<Value>(body) {
            if let Some(m) = v["error"]["message"].as_str() {
                let msg: String = m.chars().take(500).collect();
                let code = match v["error"]["code"].as_str() {
                    Some("context_length_exceeded") => "context_length_exceeded",
                    _ => "invalid_request",
                };
                return ApiError::new(status, "invalid_request_error", code, msg);
            }
        }
    }
    if matches!(status, 429 | 503) {
        let mut e = ApiError::new(
            503,
            "server_error",
            "engine_busy",
            "The model engine is busy.",
        );
        e.retry_after = Some(10);
        return e;
    }
    ApiError::new(
        502,
        "server_error",
        "engine_error",
        "The model engine returned an error.",
    )
}

async fn chat(inner: Arc<Inner>, req: Request) -> Result<Response, ApiError> {
    let key = authenticate(&inner, req.headers())?;
    if let Err(retry) = inner.take_token(&key.id) {
        let mut e = ApiError::new(
            429,
            "rate_limit_error",
            "rate_limit_exceeded",
            "Per-key rate limit exceeded.",
        );
        e.retry_after = Some(retry);
        return Err(e);
    }
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct.to_ascii_lowercase().starts_with("application/json") {
        return Err(ApiError::new(
            415,
            "invalid_request_error",
            "unsupported_media_type",
            "Content-Type must be application/json.",
        ));
    }
    let body = axum::body::to_bytes(req.into_body(), inner.cfg.max_body_bytes)
        .await
        .map_err(|_| {
            ApiError::new(
                413,
                "invalid_request_error",
                "request_too_large",
                format!("Request body exceeds {} bytes.", inner.cfg.max_body_bytes),
            )
        })?;
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad("invalid_json", "Body is not valid JSON."))?;
    let (up_body, stream) = sanitize_chat_request(parsed, &inner.cfg)?;
    if !inner.ready.load(SeqCst) {
        return Err(unavailable());
    }
    let mut log = ReqLog::new(&inner, Some(key.id.clone()), stream);
    let admission = admit(&inner).await.inspect_err(|e| {
        log.status = e.status.as_u16();
        log.outcome = "rejected";
    })?;
    let Admission::Granted(permit, queue_ms) = admission;
    log.queue_ms = Some(queue_ms);
    let payload = serde_json::to_vec(&up_body).unwrap();
    if stream {
        Ok(stream_response(inner, payload, permit, log))
    } else {
        let _active = ActiveGuard::new(&inner);
        let _permit = permit;
        let fut = async {
            let r = inner
                .client
                .post(format!(
                    "{}/v1/chat/completions",
                    inner.cfg.upstream.trim_end_matches('/')
                ))
                .bearer_auth(&inner.internal_key)
                .header(header::CONTENT_TYPE, "application/json")
                .body(payload)
                .timeout(Duration::from_secs(inner.cfg.first_byte_timeout_secs))
                .send()
                .await?;
            let st = r.status().as_u16();
            let b = r.bytes().await?;
            Ok::<_, reqwest::Error>((st, b))
        };
        let res =
            tokio::time::timeout(Duration::from_secs(inner.cfg.request_deadline_secs), fut).await;
        let (st, b) = match res {
            Err(_) => {
                log.status = 504;
                log.outcome = "deadline";
                return Err(ApiError::new(
                    504,
                    "server_error",
                    "deadline_exceeded",
                    "The request exceeded its deadline.",
                ));
            }
            Ok(Err(_)) => {
                log.status = 502;
                log.outcome = "engine_error";
                return Err(ApiError::new(
                    502,
                    "server_error",
                    "engine_error",
                    "The model engine is unreachable.",
                ));
            }
            Ok(Ok(x)) => x,
        };
        log.first_byte_ms = Some(log.started.elapsed().as_millis() as i64);
        if !(200..300).contains(&st) {
            let e = upstream_error(st, &b);
            log.status = e.status.as_u16();
            log.outcome = "upstream_error";
            return Err(e);
        }
        let v: Value = serde_json::from_slice(&b).map_err(|_| {
            ApiError::new(
                502,
                "server_error",
                "engine_error",
                "The model engine returned an invalid response.",
            )
        })?;
        log.usage(&v);
        log.status = 200;
        log.outcome = "ok";
        Ok(Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b))
            .unwrap())
    }
}

fn sse_error(e: &ApiError) -> Bytes {
    Bytes::from(format!("data: {}\n\ndata: [DONE]\n\n", e.body()))
}

fn stream_response(
    inner: Arc<Inner>,
    payload: Vec<u8>,
    permit: tokio::sync::OwnedSemaphorePermit,
    mut log: ReqLog,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let _permit = permit;
        let _active = ActiveGuard::new(&inner);
        let deadline = Instant::now() + Duration::from_secs(inner.cfg.request_deadline_secs);
        let url = format!(
            "{}/v1/chat/completions",
            inner.cfg.upstream.trim_end_matches('/')
        );
        let send = inner
            .client
            .post(url)
            .bearer_auth(&inner.internal_key)
            .header(header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send();
        tokio::pin!(send);
        let first_deadline =
            tokio::time::sleep(Duration::from_secs(inner.cfg.first_byte_timeout_secs));
        tokio::pin!(first_deadline);
        let mut ka = tokio::time::interval(Duration::from_secs(inner.cfg.keepalive_secs.max(1)));
        ka.tick().await;
        // Phase 1: wait for upstream headers, emitting SSE comments so intermediaries
        // with first-byte timeouts keep the connection open during long prefill.
        let resp = loop {
            tokio::select! {
                r = &mut send => break r,
                _ = ka.tick() => { if tx.send(Ok(Bytes::from_static(b": keepalive\n\n"))).await.is_err() { return; } }
                _ = tx.closed() => return, // client left: dropping `send` cancels upstream
                _ = &mut first_deadline => {
                    log.status = 504; log.outcome = "deadline";
                    let _ = tx.send(Ok(sse_error(&ApiError::new(504, "server_error", "deadline_exceeded", "Timed out waiting for the model.")))).await;
                    return;
                }
            }
        };
        let resp = match resp {
            Ok(r) => r,
            Err(_) => {
                log.status = 502;
                log.outcome = "engine_error";
                let _ = tx
                    .send(Ok(sse_error(&ApiError::new(
                        502,
                        "server_error",
                        "engine_error",
                        "The model engine is unreachable.",
                    ))))
                    .await;
                return;
            }
        };
        let st = resp.status().as_u16();
        if !(200..300).contains(&st) {
            let b = resp.bytes().await.unwrap_or_default();
            let e = upstream_error(st, &b);
            log.status = e.status.as_u16();
            log.outcome = "upstream_error";
            let _ = tx.send(Ok(sse_error(&e))).await;
            return;
        }
        log.status = 200;
        let mut up = resp.bytes_stream();
        let idle = Duration::from_secs(inner.cfg.stream_idle_timeout_secs);
        let mut tail: Vec<u8> = Vec::new();
        let mut first = true;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                log.outcome = "deadline";
                let _ = tx
                    .send(Ok(sse_error(&ApiError::new(
                        504,
                        "server_error",
                        "deadline_exceeded",
                        "The stream exceeded its deadline.",
                    ))))
                    .await;
                return;
            }
            // Before the first body byte the wait is the (long) first-byte timeout and SSE
            // comments keep intermediaries alive; afterwards the idle timeout applies.
            let wait = if first {
                Duration::from_secs(inner.cfg.first_byte_timeout_secs)
            } else {
                idle
            };
            let next = loop {
                tokio::select! {
                    n = tokio::time::timeout(wait.min(remaining), up.next()) => break n,
                    _ = tx.closed() => return,
                    _ = ka.tick(), if first => {
                        if tx.send(Ok(Bytes::from_static(b": keepalive\n\n"))).await.is_err() { return; }
                    }
                }
            };
            match next {
                Err(_) => {
                    log.outcome = "idle_timeout";
                    let _ = tx
                        .send(Ok(sse_error(&ApiError::new(
                            504,
                            "server_error",
                            "stream_stalled",
                            "The model stopped producing output.",
                        ))))
                        .await;
                    return;
                }
                Ok(None) => {
                    log.outcome = "ok";
                    break;
                }
                Ok(Some(Err(_))) => {
                    log.outcome = "engine_error";
                    let _ = tx
                        .send(Ok(sse_error(&ApiError::new(
                            502,
                            "server_error",
                            "engine_error",
                            "The model stream failed.",
                        ))))
                        .await;
                    return;
                }
                Ok(Some(Ok(chunk))) => {
                    if first {
                        log.first_byte_ms = Some(log.started.elapsed().as_millis() as i64);
                        first = false;
                    }
                    tail.extend_from_slice(&chunk);
                    if tail.len() > 4096 {
                        let cut = tail.len() - 4096;
                        tail.drain(..cut);
                    }
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
            }
        }
        // Verified usage only if the upstream sent a usage event (stream_options.include_usage).
        if let Some(line) = String::from_utf8_lossy(&tail)
            .lines()
            .rev()
            .find(|l| l.starts_with("data: {") && l.contains("\"usage\""))
        {
            if let Ok(v) = serde_json::from_str::<Value>(&line[6..]) {
                log.usage(&v);
            }
        }
    });
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|c| (c, rx))
    }));
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}
