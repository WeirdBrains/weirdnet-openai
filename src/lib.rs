//! A loopback-only OpenAI-compatible bridge over a `weirdnet-agent` [`Pool`].
//!
//! Local tools that speak the OpenAI API (editors, notebooks, `curl`) reach the user's
//! inference nodes through it: `/v1/chat/completions` and `/v1/completions` (streaming or not),
//! `/v1/models` (the pool catalog), `/v1/embeddings`, `/v1/audio/speech` and
//! `/v1/audio/transcriptions`, plus Prometheus `/metrics`.
//!
//! It binds `127.0.0.1` only, checks the `Host` header against its own address (DNS rebinding),
//! requires a bearer token of at least 16 characters (compared in constant time), and sends no
//! CORS headers, so web pages can't use it. Answers carry `x-weirdnet-model`, and
//! `x-weirdnet-cache: hit` and `x-weirdnet-standin` when the pool says so; a request with
//! `x-wait: 1` waits for its model instead of a stand-in. A request skips the nodes' answer cache
//! with `"cache": false` or `x-weirdnet-cache: off`, and refreshes it with `"cache": "refresh"`
//! or `x-weirdnet-cache: refresh` (infer/2 `Request::cache`, `weirdnet-agent` 0.5.1).
//!
//! Chat messages may carry image (`image_url`, a `data:image/...` URL), audio (`input_audio`) and
//! video (`video_url`, a `data:video/...` URL; remote URLs are refused, never fetched) parts. They
//! go to the pool unchanged, which sends them only to a model whose catalog entry takes them; when
//! none does the answer is 400. `/v1/models` lists `weirdnet.vision`, `weirdnet.audio` and
//! `weirdnet.video` per model (also together as `weirdnet.inputs`), `weirdnet.purpose` (`chat`,
//! `embed`, `transcribe`, `speak`, ...), `weirdnet.speech` for a speaking model that reports a
//! profile (catalog/1 `CatalogModel::speech`, `weirdnet-agent` 0.5.11), `weirdnet.base` for a LoRA adapter (its base model), and
//! `weirdnet.origin` (`hub`, `upload` or `train`) and `weirdnet.license` when the node says
//! (catalog/1 `CatalogModel::origin`/`license`, `weirdnet-agent` 0.5.4; `null` otherwise). A body's
//! `"adapter_scale"` sets how strongly an adapter applies for that request (infer/2
//! `Request::adapter_scale`, `weirdnet-agent` 0.5.2; llama.cpp nodes only).
//!
//! Bodies may be up to [`DEFAULT_MAX_REQUEST_BYTES`] (32 MiB) unless the bridge is set otherwise
//! ([`Bridge::with_max_request_bytes`]). A larger body is refused with 413
//! (`request_too_large`) whose message names the limit, and the `x-weirdnet-max-request-bytes`
//! header carries it. `/v1/models` reports it per model as `weirdnet.max_request_bytes` (the
//! smaller of the bridge's and the node's), with the model's media limits under
//! `weirdnet.limits` (catalog/1 `CatalogModel::limits`, `weirdnet-agent` 0.5.7; only the limits the
//! node knows).
//!
//! `POST /v1/audio/speech` speaks text over infer/2 `Speak` ([`request::speech`] lists the body):
//! the request is checked against the catalog first, so a voice the model lacks (OpenAI's `alloy`,
//! say) is a 400 that lists the model's presets, never mapped to another voice. The answer's
//! `Content-Type` is the real format (`audio/pcm;rate=24000` for raw PCM), with
//! `x-audio-sample-rate` when the node knows it; `stream_format: "audio"` streams the bytes as they
//! come. `POST /v1/audio/transcriptions` (multipart: `file`, `model`, `language`, `prompt`,
//! `response_format` `json`, `verbose_json`, `text`, `srt` or `vtt`, and
//! `timestamp_granularities[]=segment`) runs infer/2 `Transcribe`. Both are bounded by the same
//! request limit (a 413 with `x-weirdnet-max-request-bytes`); neither is cached.
//!
//! `/v1/completions` sends the prompt as an infer/2 raw prompt (`weirdnet-agent` 0.4.1); nodes
//! that can't take one answer 501. A streamed answer waits up to [`EARLY_META_WAIT`] after a node
//! accepts it for early metadata, so `x-weirdnet-cache: hit` is sent on streams too.

mod audio;
pub mod request;

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use weirdnet_agent::CancelToken;
use weirdnet_agent::remote::RemoteError;
use weirdnet_agent::remote::infer::v2::{Meta, Usage};
use weirdnet_agent::remote::pool::{Answer, EventSink, Interrupted, NoEvents, Pool, PoolEvent};

/// How long a streamed answer's headers wait, after a node accepted it, for early metadata (a
/// cache hit) or the first output. A node that sends neither in time gets headers without it.
pub const EARLY_META_WAIT: Duration = Duration::from_millis(1500);

/// The largest request body a bridge accepts unless set otherwise: 32 MiB.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 32 << 20;

/// Where the bridge listens and the token it requires.
#[derive(Clone)]
pub struct BridgeOptions {
    /// Port on 127.0.0.1; 0 picks a free one (tests).
    pub port: u16,
    pub token: String,
}

impl BridgeOptions {
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self { port, token: token.into() }
    }
}

impl std::fmt::Debug for BridgeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeOptions").field("port", &self.port).field("token", &"<redacted>").finish()
    }
}

/// A bound, not yet serving, bridge.
pub struct Bridge {
    listener: tokio::net::TcpListener,
    token: String,
    max_request_bytes: usize,
}

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    errors: AtomicU64,
    cache_hits: AtomicU64,
    standins: AtomicU64,
}

struct AppState {
    pool: Arc<Pool>,
    token: String,
    port: u16,
    max_request_bytes: usize,
    metrics: Metrics,
}

impl Bridge {
    /// Binds `127.0.0.1:port`. Refuses a token shorter than 16 characters.
    pub async fn bind(opts: BridgeOptions) -> std::io::Result<Self> {
        if opts.token.len() < 16 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the bridge token must be at least 16 characters"));
        }
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], opts.port))).await?;
        Ok(Self { listener, token: opts.token, max_request_bytes: DEFAULT_MAX_REQUEST_BYTES })
    }

    /// The largest request body accepted, in bytes (at least 1; default
    /// [`DEFAULT_MAX_REQUEST_BYTES`]). Larger bodies get 413 naming the limit. infer/2 carries at
    /// most `weirdnet_agent::remote::codec::MAX_FRAME` (32 MiB) per message, so a higher limit
    /// only helps bodies that shrink on the way to the pool.
    pub fn with_max_request_bytes(mut self, bytes: usize) -> Self {
        self.max_request_bytes = bytes.max(1);
        self
    }

    /// See [`Bridge::with_max_request_bytes`].
    pub fn max_request_bytes(&self) -> usize {
        self.max_request_bytes
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves until `shutdown` resolves.
    pub async fn serve(self, pool: Arc<Pool>, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
        let port = self.listener.local_addr()?.port();
        let state = Arc::new(AppState { pool, token: self.token, port, max_request_bytes: self.max_request_bytes, metrics: Metrics::default() });
        axum::serve(self.listener, router(state)).with_graceful_shutdown(shutdown).await
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/audio/speech", post(audio::speech))
        .route("/v1/audio/transcriptions", post(audio::transcriptions))
        .route("/metrics", get(metrics))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(state.max_request_bytes))
        .layer(middleware::from_fn_with_state(state.clone(), size_limit))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Host check, then the token, on every request.
async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if host != format!("127.0.0.1:{}", st.port) && host != format!("localhost:{}", st.port) {
        return error(StatusCode::FORBIDDEN, "forbidden_host", "the bridge answers only 127.0.0.1 and localhost");
    }
    let presented = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).unwrap_or("");
    if !same(presented.as_bytes(), st.token.as_bytes()) {
        return error(StatusCode::UNAUTHORIZED, "invalid_api_key", "missing or wrong bridge token");
    }
    st.metrics.requests.fetch_add(1, Ordering::Relaxed);
    next.run(req).await
}

/// How much of a body over the limit is still read (and dropped), so the client finishes sending
/// and reads the 413 instead of a broken connection; past this the connection is just closed.
const DRAIN_BYTES: u64 = 64 << 20;

/// Bodies over the limit get a 413 that names it: the body is read up to the limit (or not at
/// all when `Content-Length` is over it), the rest is drained, and handlers only ever see bodies
/// that fit.
async fn size_limit(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let limit = st.max_request_bytes;
    let declared = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let mut over = declared.is_some_and(|n| n > limit as u64);
    let (parts, body) = req.into_parts();
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(declared.map_or(0, |n| n.min(limit as u64) as usize));
    let mut seen: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return error(StatusCode::BAD_REQUEST, "bad_request", "could not read the request body");
        };
        seen += chunk.len() as u64;
        if !over && buf.len() + chunk.len() > limit {
            over = true;
            buf = Vec::new();
        }
        if over {
            if seen > (limit as u64).saturating_add(DRAIN_BYTES) {
                break;
            }
            continue;
        }
        buf.extend_from_slice(&chunk);
    }
    if over {
        return too_large(&st);
    }
    next.run(Request::from_parts(parts, axum::body::Body::from(buf))).await
}

fn too_large(st: &AppState) -> Response {
    st.metrics.errors.fetch_add(1, Ordering::Relaxed);
    let limit = st.max_request_bytes;
    let message = format!(
        "the request body is larger than this bridge's limit of {limit} bytes ({}); send less, or raise the bridge's max_request_bytes",
        human_bytes(limit)
    );
    let mut r = error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", &message);
    r.headers_mut().insert(HeaderName::from_static("x-weirdnet-max-request-bytes"), HeaderValue::from(limit as u64));
    r
}

/// `32 MiB`, `512 KiB` or `1000 bytes`.
fn human_bytes(n: usize) -> String {
    match n {
        n if n >= 1 << 20 && n % (1 << 20) == 0 => format!("{} MiB", n >> 20),
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / f64::from(1 << 20)),
        n if n >= 1 << 10 && n % (1 << 10) == 0 => format!("{} KiB", n >> 10),
        n => format!("{n} bytes"),
    }
}

fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let kind = if status.is_server_error() { "server_error" } else { "invalid_request_error" };
    (status, axum::Json(json!({"error": {"message": message, "type": kind, "code": code}}))).into_response()
}

/// The HTTP status and code for a pool error.
pub fn status_of(e: &RemoteError) -> (StatusCode, &'static str) {
    match e {
        RemoteError::Unavailable { message } if message.starts_with("unknown model") => (StatusCode::NOT_FOUND, "model_not_found"),
        RemoteError::BadRequest { .. } => (StatusCode::BAD_REQUEST, "bad_request"),
        RemoteError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy"),
        RemoteError::Unavailable { .. } | RemoteError::Unreachable { .. } => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        RemoteError::Refused { .. } => (StatusCode::FORBIDDEN, "refused"),
        RemoteError::Unsupported { .. } => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
        RemoteError::Timeout | RemoteError::Stalled { .. } => (StatusCode::GATEWAY_TIMEOUT, "timeout"),
        _ => (StatusCode::BAD_GATEWAY, "engine_error"),
    }
}

fn pool_error(st: &AppState, e: &RemoteError) -> Response {
    st.metrics.errors.fetch_add(1, Ordering::Relaxed);
    let (status, code) = status_of(e);
    let mut r = error(status, code, &e.to_string());
    if matches!(e, RemoteError::Busy) {
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(1));
    }
    r
}

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
}

async fn metrics(State(st): State<Arc<AppState>>) -> Response {
    let m = &st.metrics;
    let body = format!(
        "# TYPE weirdnet_bridge_requests_total counter\nweirdnet_bridge_requests_total {}\n# TYPE weirdnet_bridge_errors_total counter\nweirdnet_bridge_errors_total {}\n# TYPE weirdnet_bridge_cache_hits_total counter\nweirdnet_bridge_cache_hits_total {}\n# TYPE weirdnet_bridge_standin_total counter\nweirdnet_bridge_standin_total {}\n",
        m.requests.load(Ordering::Relaxed),
        m.errors.load(Ordering::Relaxed),
        m.cache_hits.load(Ordering::Relaxed),
        m.standins.load(Ordering::Relaxed)
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

async fn models(State(st): State<Arc<AppState>>) -> Response {
    match st.pool.catalog().await {
        Ok(catalog) => {
            let data: Vec<Value> = catalog
                .models
                .iter()
                .map(|m| {
                    let max_request_bytes = m.limits.max_request_bytes.map_or(st.max_request_bytes as u64, |n| n.min(st.max_request_bytes as u64));
                    let mut limits = m.limits;
                    limits.max_request_bytes = Some(max_request_bytes);
                    let mut v = json!({
                        "id": m.id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "weirdnet",
                        "weirdnet": {
                            "state": m.state, "purpose": m.purpose, "replicas": m.replicas, "context": m.context, "revision": m.revision,
                            "vision": m.inputs.vision, "audio": m.inputs.audio, "video": m.inputs.video,
                            "inputs": {"vision": m.inputs.vision, "audio": m.inputs.audio, "video": m.inputs.video},
                            "base": m.base, "origin": m.origin, "license": m.license,
                            "max_request_bytes": max_request_bytes, "limits": limits,
                        },
                    });
                    if let Some(speech) = &m.speech {
                        v["weirdnet"]["speech"] = json!(speech);
                    }
                    v
                })
                .collect();
            axum::Json(json!({"object": "list", "data": data})).into_response()
        }
        Err(e) => pool_error(&st, &e),
    }
}

/// The `x-weirdnet-cache` header with the body's `cache`: the stricter wins. A bad header is a
/// 400 (the returned response), so an eval that asked to skip the cache never silently reads it.
fn apply_cache_header(h: &HeaderMap, req: &mut weirdnet_agent::remote::infer::v2::Request) -> Option<Response> {
    let v = h.get("x-weirdnet-cache")?;
    match v.to_str().ok().map(request::cache_header) {
        Some(Ok(mode)) => {
            req.cache = request::stricter(req.cache, mode);
            None
        }
        Some(Err(e)) => Some(error(StatusCode::BAD_REQUEST, "bad_request", &e.0)),
        None => Some(error(StatusCode::BAD_REQUEST, "bad_request", "`x-weirdnet-cache` must be off, refresh or use")),
    }
}

/// `x-wait: 1` asks to wait for the requested model instead of a stand-in.
fn wants_wait(h: &HeaderMap) -> bool {
    h.get("x-wait").and_then(|v| v.to_str().ok()).is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
}

fn json_body(body: &Bytes) -> Result<Value, String> {
    serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))
}

fn header_value(v: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(v).ok()
}

/// `x-weirdnet-*` headers.
fn reply_headers(st: &AppState, model: &str, cache_hit: bool, standin: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(v) = header_value(model) {
        headers.insert(HeaderName::from_static("x-weirdnet-model"), v);
    }
    if cache_hit {
        st.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        headers.insert(HeaderName::from_static("x-weirdnet-cache"), HeaderValue::from_static("hit"));
    }
    if let Some(v) = standin.and_then(header_value) {
        st.metrics.standins.fetch_add(1, Ordering::Relaxed);
        headers.insert(HeaderName::from_static("x-weirdnet-standin"), v);
    }
    headers
}

fn usage_json(u: Option<Usage>) -> Value {
    let u = u.unwrap_or_default();
    let mut v =
        json!({"prompt_tokens": u.prompt_tokens, "completion_tokens": u.completion_tokens, "total_tokens": u.prompt_tokens + u.completion_tokens});
    if u.cached_tokens > 0 {
        v["prompt_tokens_details"] = json!({"cached_tokens": u.cached_tokens});
    }
    if u.reasoning_tokens > 0 {
        v["completion_tokens_details"] = json!({"reasoning_tokens": u.reasoning_tokens});
    }
    v
}

fn weirdnet_json(meta: &Meta, standin: Option<&str>) -> Value {
    json!({"standin": standin.or(meta.standin.as_deref()), "cache_hit": meta.cache_hit, "served_by": meta.served_by})
}

/// `/v1/completions`: a raw prompt over infer/2.
async fn completions(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> Response {
    let body = match json_body(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e),
    };
    let (mut req, stream) = match request::completion(body) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e.0),
    };
    req.wait_for_model |= wants_wait(&h);
    if let Some(r) = apply_cache_header(&h, &mut req) {
        return r;
    }
    answer(st, req, stream, Shape::Text).await
}

async fn embeddings(State(st): State<Arc<AppState>>, body: Bytes) -> Response {
    let body = match json_body(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e),
    };
    let req = match request::embed(body) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e.0),
    };
    match st.pool.embed(req, &CancelToken::new()).await {
        Ok(e) => {
            let headers = reply_headers(&st, &e.model, e.meta.cache_hit, e.meta.standin.as_deref());
            let data: Vec<Value> = e.vectors.iter().enumerate().map(|(i, v)| json!({"object": "embedding", "index": i, "embedding": v})).collect();
            // Nodes before weirdnet-agent 0.4.1 report no count: 0.
            let tokens = e.usage.map(|u| u.prompt_tokens).unwrap_or(0);
            let body = json!({"object": "list", "model": e.model, "data": data, "usage": {"prompt_tokens": tokens, "total_tokens": tokens}});
            (headers, axum::Json(body)).into_response()
        }
        Err(e) => pool_error(&st, &e),
    }
}

/// Cancels the pool request when the HTTP response (or stream) is dropped.
struct CancelOnDrop(CancelToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Pool events into a channel, for streaming.
struct Forward(mpsc::UnboundedSender<Step>);

enum Step {
    Event(PoolEvent),
    End(Box<Result<Answer, Interrupted>>),
}

impl EventSink for Forward {
    fn event(&self, event: PoolEvent) {
        let _ = self.0.send(Step::Event(event));
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn chat(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> Response {
    let body = match json_body(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e),
    };
    let (mut req, stream) = match request::chat(body) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e.0),
    };
    req.wait_for_model |= wants_wait(&h);
    if let Some(r) = apply_cache_header(&h, &mut req) {
        return r;
    }
    answer(st, req, stream, Shape::Chat).await
}

/// Which OpenAI answer shape: chat completions or text completions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Chat,
    Text,
}

impl Shape {
    fn id(self) -> String {
        match self {
            Shape::Chat => format!("chatcmpl-{}", random_hex(12)),
            Shape::Text => format!("cmpl-{}", random_hex(12)),
        }
    }

    fn whole(self, id: &str, created: u64, a: &Answer) -> Value {
        match self {
            Shape::Chat => completion(id, created, a),
            Shape::Text => text_completion(id, created, a),
        }
    }
}

/// Runs `req` on the pool and answers in `shape`, whole or streamed.
async fn answer(st: Arc<AppState>, req: weirdnet_agent::remote::infer::v2::Request, stream: bool, shape: Shape) -> Response {
    let id = shape.id();
    let created = now_secs();
    let cancel = CancelToken::new();
    if !stream {
        let guard = CancelOnDrop(cancel.clone());
        let answer = st.pool.infer(req, &NoEvents, &cancel).await;
        drop(guard);
        return match answer {
            Ok(a) => {
                let headers = reply_headers(&st, &a.model, a.meta.cache_hit, a.meta.standin.as_deref());
                let mut v = shape.whole(&id, created, &a);
                v["weirdnet"] = weirdnet_json(&a.meta, None);
                (headers, axum::Json(v)).into_response()
            }
            Err(i) => pool_error(&st, &i.error),
        };
    }

    // Streaming: run the pool request in a task; answer once a node accepted it (so failures
    // before that are proper HTTP errors and `x-weirdnet-model` is known), and the node sent
    // early metadata or its first output, or EARLY_META_WAIT passed.
    let requested = req.model.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let pool = st.pool.clone();
    let c = cancel.clone();
    tokio::spawn(async move {
        let sink = Forward(tx.clone());
        let r = pool.infer(req, &sink, &c).await;
        let _ = tx.send(Step::End(Box::new(r)));
    });
    let guard = CancelOnDrop(cancel);
    let mut pending = Vec::new();
    let mut ended = false;
    let model = loop {
        match rx.recv().await {
            Some(Step::Event(PoolEvent::Accepted { model, .. })) => break model,
            Some(Step::Event(ev)) => pending.push(Step::Event(ev)),
            Some(Step::End(r)) => match *r {
                Ok(a) => {
                    let model = a.model.clone();
                    pending.push(Step::End(Box::new(Ok(a))));
                    ended = true;
                    break model;
                }
                Err(i) => return pool_error(&st, &i.error),
            },
            None => return pool_error(&st, &RemoteError::Closed),
        }
    };
    let mut early = Meta::default();
    if ended {
        if let Some(Step::End(r)) = pending.last()
            && let Ok(a) = r.as_ref()
        {
            early = a.meta.clone();
        }
    } else {
        let deadline = tokio::time::Instant::now() + EARLY_META_WAIT;
        loop {
            let step = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) | Ok(None) => break,
                Ok(Some(s)) => s,
            };
            match step {
                Step::Event(PoolEvent::Meta(m)) => {
                    early = m;
                    break;
                }
                Step::Event(ev @ (PoolEvent::Text(_) | PoolEvent::Reasoning(_) | PoolEvent::ToolCall(_))) => {
                    pending.push(Step::Event(ev));
                    break;
                }
                Step::End(r) => {
                    if let Ok(a) = r.as_ref() {
                        early = a.meta.clone();
                    }
                    pending.push(Step::End(r));
                    break;
                }
                other => pending.push(other),
            }
        }
    }
    let standin = requested.as_deref().filter(|r| *r != model).map(|_| model.clone()).or_else(|| early.standin.clone());
    let headers = reply_headers(&st, &model, early.cache_hit, standin.as_deref());
    let counted = early.cache_hit;
    let chunk = {
        let (id, model) = (id.clone(), model.clone());
        move |delta: Value, finish: Option<&str>, extra: Option<(Value, Value)>| -> Result<SseEvent, Infallible> {
            let mut v = match shape {
                Shape::Chat => {
                    json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
                }
                Shape::Text => {
                    let text = delta.get("content").and_then(Value::as_str).unwrap_or("");
                    json!({"id": id, "object": "text_completion", "created": created, "model": model, "choices": [{"index": 0, "text": text, "logprobs": null, "finish_reason": finish}]})
                }
            };
            if let Some((usage, weirdnet)) = extra {
                v["usage"] = usage;
                v["weirdnet"] = weirdnet;
            }
            Ok(SseEvent::default().data(v.to_string()))
        }
    };
    let st2 = st.clone();
    let body = async_stream(move |out| async move {
        let _guard = guard;
        let mut tool_index = 0usize;
        let mut usage = None;
        if shape == Shape::Chat {
            out(chunk(json!({"role": "assistant", "content": ""}), None, None));
        }
        let mut steps = pending.into_iter();
        loop {
            let step = match steps.next() {
                Some(s) => s,
                None => match rx.recv().await {
                    Some(s) => s,
                    None => break,
                },
            };
            match step {
                Step::Event(PoolEvent::Text(t)) => out(chunk(json!({"content": t}), None, None)),
                Step::Event(PoolEvent::Reasoning(t)) if shape == Shape::Chat => out(chunk(json!({"reasoning_content": t}), None, None)),
                Step::Event(PoolEvent::ToolCall(call)) if shape == Shape::Chat => {
                    let call = json!({"index": tool_index, "id": call.id, "type": "function", "function": {"name": call.name, "arguments": call.arguments.to_string()}});
                    tool_index += 1;
                    out(chunk(json!({"tool_calls": [call]}), None, None));
                }
                Step::Event(PoolEvent::Usage(u)) => usage = Some(u),
                Step::Event(_) => {}
                Step::End(r) => {
                    match *r {
                        Ok(a) => {
                            if a.meta.cache_hit && !counted {
                                st2.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
                            }
                            let finish = a.finish_reason.as_deref().unwrap_or("stop");
                            let extra = (usage_json(a.usage.or(usage)), weirdnet_json(&a.meta, standin.as_deref()));
                            out(chunk(json!({}), Some(finish), Some(extra)));
                            out(Ok(SseEvent::default().data("[DONE]")));
                        }
                        Err(i) => {
                            st2.metrics.errors.fetch_add(1, Ordering::Relaxed);
                            let (_, code) = status_of(&i.error);
                            let e = json!({"error": {"message": i.error.to_string(), "type": "server_error", "code": code}});
                            out(Ok(SseEvent::default().data(e.to_string())));
                        }
                    }
                    break;
                }
            }
        }
    });
    (headers, Sse::new(body).keep_alive(KeepAlive::default())).into_response()
}

/// A stream fed by an async producer through a channel.
fn async_stream<F, Fut>(produce: F) -> impl futures::Stream<Item = Result<SseEvent, Infallible>> + Send
where
    F: FnOnce(Box<dyn Fn(Result<SseEvent, Infallible>) + Send + Sync>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(produce(Box::new(move |ev| {
        let _ = tx.send(ev);
    })));
    // Dropping the response stream aborts the producer, which cancels the pool request.
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let abort = Abort(task);
    futures::stream::unfold((rx, abort), |(mut rx, abort)| async move { rx.recv().await.map(|ev| (ev, (rx, abort))) })
}

fn completion(id: &str, created: u64, a: &Answer) -> Value {
    // A continued answer (`continue_final_message`) returns only the new text, as vLLM does.
    let mut message = json!({"role": "assistant", "content": a.continuation()});
    if !a.reasoning.is_empty() {
        message["reasoning_content"] = a.reasoning.clone().into();
    }
    if !a.tool_calls.is_empty() {
        message["tool_calls"] = a
            .tool_calls
            .iter()
            .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments.to_string()}}))
            .collect::<Vec<_>>()
            .into();
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": a.model,
        "choices": [{"index": 0, "message": message, "finish_reason": a.finish_reason.as_deref().unwrap_or("stop")}],
        "usage": usage_json(a.usage),
    })
}

fn text_completion(id: &str, created: u64, a: &Answer) -> Value {
    json!({
        "id": id,
        "object": "text_completion",
        "created": created,
        "model": a.model,
        "choices": [{"index": 0, "text": a.text, "logprobs": null, "finish_reason": a.finish_reason.as_deref().unwrap_or("stop")}],
        "usage": usage_json(a.usage),
    })
}

/// A new random bridge token (32 bytes, hex).
pub fn new_token() -> String {
    random_hex(32)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the OS RNG is available");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
