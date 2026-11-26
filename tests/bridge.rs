//! The bridge over a real `weirdnet-agent` pool on memory channels, with a fake inference node.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use weirdnet_agent::remote::catalog::{self, CatalogFeed, CatalogModel, ModelState, Purpose};
use weirdnet_agent::remote::infer::v2::{self, CacheMode, EmbedBackend, Finish, InferBackend, Meta, Output, Request, Usage};
use weirdnet_agent::remote::infer::{Admission, InferHost};
use weirdnet_agent::remote::pool::Pool;
use weirdnet_agent::remote::{Caller, MemoryDialer, RemoteError};
use weirdnet_agent::{CancelToken, ContentPart, MediaLimits, Modalities, ModelBackend, ToolCall};
use weirdnet_openai::{Bridge, BridgeOptions, DEFAULT_MAX_REQUEST_BYTES, new_token};

const TOKEN: &str = "0123456789abcdef0123";

/// "big" is cold: a stand-in ("small") answers unless the request waits; "slow" streams until
/// cancelled; "cached" is a cache hit it reports early, "late-cached" one it reports only at the
/// end (a node before 0.4.1); "chat-only" refuses raw prompts; with tools, it calls the first one;
/// a raw prompt is continued with " upon a time"; a request that skips or refreshes the answer
/// cache gets its cache mode back as the answer.
struct Fake {
    model: String,
    cancelled: Arc<Mutex<u32>>,
}

#[async_trait]
impl InferBackend for Fake {
    fn name(&self) -> String {
        self.model.clone()
    }

    /// Everything but "chat-only" continues an assistant prefix.
    fn resumes(&self) -> bool {
        self.model != "chat-only"
    }

    async fn infer(&self, request: &Request, out: &Output, cancel: &CancelToken) -> Result<Finish, RemoteError> {
        if self.model == "slow" {
            loop {
                out.text("tick ");
                tokio::select! {
                    _ = cancel.cancelled() => {
                        *self.cancelled.lock().unwrap() += 1;
                        return Err(RemoteError::Cancelled);
                    }
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        }
        if request.cache != CacheMode::Use {
            out.text(format!("cache {:?}", request.cache));
            return Ok(Finish::new("stop"));
        }
        if let Some(prompt) = request.raw_prompt() {
            if self.model == "chat-only" {
                return Err(RemoteError::Unsupported { message: "this backend takes messages, not a raw prompt".into() });
            }
            assert_eq!(prompt, "Once");
            out.text(" upon");
            out.text(" a time");
            return Ok(Finish::new("length").with_usage(Usage::new(1, 3)));
        }
        if self.model == "vision" {
            let m = &request.inline_messages().unwrap()[0];
            let images = m.content.parts().iter().filter(|p| matches!(p, ContentPart::ImageUrl { .. })).count();
            out.text(format!("{} [{images} image(s)]", m.text()));
            return Ok(Finish::new("stop").with_usage(Usage::new(5, 3)));
        }
        if let Some(p) = &request.assistant_prefix {
            assert_eq!(p.text, "Hel");
            out.text("lo there");
            return Ok(Finish::new("stop").with_usage(Usage::new(3, 2)));
        }
        if self.model == "cached" {
            let mut meta = Meta::default();
            meta.cache_hit = true;
            out.meta(meta);
        }
        if let Some(tool) = request.tools.first() {
            out.tool_call(ToolCall { id: "call_1".into(), name: tool.name.clone(), arguments: json!({"q": 1}) });
            return Ok(Finish::new("tool_calls").with_usage(Usage::new(3, 1)));
        }
        out.reasoning("hm");
        out.text("Hel");
        out.text("lo");
        let mut f = Finish::new("stop").with_usage(Usage::new(2, 2));
        if self.model == "small" && request.model.as_deref() == Some("big") {
            f.meta.standin = Some("small".into());
        }
        f.meta.cache_hit = self.model == "cached" || self.model == "late-cached";
        Ok(f)
    }
}

#[async_trait]
impl EmbedBackend for Fake {
    fn name(&self) -> String {
        self.model.clone()
    }
    async fn embed(&self, input: &str, _cancel: &CancelToken) -> Result<Vec<f32>, RemoteError> {
        Ok(vec![input.len() as f32, 1.0])
    }
    /// One token per character.
    async fn embed_counted(&self, input: &str, cancel: &CancelToken) -> Result<(Vec<f32>, Option<u64>), RemoteError> {
        Ok((self.embed(input, cancel).await?, Some(input.len() as u64)))
    }
}

struct Host {
    cancelled: Arc<Mutex<u32>>,
}

impl InferHost for Host {
    fn authorize(&self, _caller: &Caller) -> Result<(), RemoteError> {
        Ok(())
    }
    fn open(&self, _caller: &Caller, _model: Option<&str>) -> Result<Arc<dyn ModelBackend>, RemoteError> {
        Err(RemoteError::Unsupported { message: "infer/2 only".into() })
    }
    fn open_request(&self, _caller: &Caller, request: &Request) -> Result<Arc<dyn InferBackend>, RemoteError> {
        let model = match request.model.as_deref() {
            None | Some("m") => "m",
            Some("big") if !request.wait_for_model => "small",
            Some("big") => "big",
            Some("slow") => "slow",
            Some("cached") => "cached",
            Some("late-cached") => "late-cached",
            Some("chat-only") => "chat-only",
            Some("vision") => "vision",
            Some(other) => return Err(RemoteError::unavailable(format!("unknown model `{other}`"))),
        };
        Ok(Arc::new(Fake { model: model.into(), cancelled: self.cancelled.clone() }))
    }
    fn open_embed(&self, _caller: &Caller, model: Option<&str>) -> Result<Arc<dyn EmbedBackend>, RemoteError> {
        if model == Some("chat-only") {
            return Err(RemoteError::BadRequest { message: "chat-only is not an embedding model".into() });
        }
        Ok(Arc::new(Fake { model: model.unwrap_or("e").into(), cancelled: self.cancelled.clone() }))
    }
}

async fn start() -> (std::net::SocketAddr, Arc<Mutex<u32>>) {
    start_with(None).await
}

/// With `max_request_bytes`, the bridge's request limit is set to it.
async fn start_with(max_request_bytes: Option<usize>) -> (std::net::SocketAddr, Arc<Mutex<u32>>) {
    let cancelled = Arc::new(Mutex::new(0));
    let host: Arc<dyn InferHost> = Arc::new(Host { cancelled: cancelled.clone() });
    let feed = CatalogFeed::new(Arc::new(|_: &Caller| Ok(())));
    feed.publish(vec![
        CatalogModel::new("m", Purpose::Chat, ModelState::Warm).with_replicas(1),
        CatalogModel::new("vision", Purpose::Chat, ModelState::Loadable)
            .with_inputs(Modalities::VISION)
            .with_limits(MediaLimits::new().with_max_images(4).with_image_tokens(256).with_image_edge(896).with_max_request_bytes(1 << 30)),
        CatalogModel::new("m-tune", Purpose::Chat, ModelState::Loadable).with_base("m").with_origin("train").with_license("Apache-2.0"),
    ]);
    let dialer = MemoryDialer::new();
    let admission = Admission::with_queue(4, 4);
    dialer.route("inference", move |ch, proto| {
        let (h, a, f) = (host.clone(), admission.clone(), feed.clone());
        tokio::spawn(async move {
            let caller = Caller::new("app");
            if proto == v2::PROTOCOL {
                let _ = v2::serve(ch, caller, h.as_ref(), &a).await;
            } else if proto == catalog::PROTOCOL {
                let _ = catalog::serve(ch, caller, &f).await;
            }
        });
    });
    let pool = Arc::new(Pool::new(Arc::new(dialer), vec!["inference".to_string()]));
    let mut bridge = Bridge::bind(BridgeOptions::new(0, TOKEN)).await.unwrap();
    assert_eq!(bridge.max_request_bytes(), DEFAULT_MAX_REQUEST_BYTES);
    if let Some(n) = max_request_bytes {
        bridge = bridge.with_max_request_bytes(n);
    }
    let addr = bridge.local_addr().unwrap();
    tokio::spawn(bridge.serve(pool, std::future::pending()));
    (addr, cancelled)
}

#[tokio::test(flavor = "multi_thread")]
async fn token_host_check_and_no_cors() {
    assert!(Bridge::bind(BridgeOptions::new(0, "short")).await.is_err());
    assert_eq!(new_token().len(), 64);
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/models");
    assert_eq!(c.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(c.get(&url).bearer_auth("wrong-token-wrong-token").send().await.unwrap().status(), 401);
    let r = c.get(&url).bearer_auth(TOKEN).header("host", "evil.example:80").send().await.unwrap();
    assert_eq!(r.status(), 403, "DNS rebinding guard");
    let r = c.get(&url).bearer_auth(TOKEN).header("origin", "https://evil.example").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("access-control-allow-origin").is_none());
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["data"][0]["id"], "m");
    assert_eq!(v["data"][0]["weirdnet"]["state"], "warm");
    let r = c.request(reqwest::Method::OPTIONS, format!("http://{addr}/v1/chat/completions")).bearer_auth(TOKEN).send().await.unwrap();
    assert!(r.headers().get("access-control-allow-origin").is_none());
    assert_eq!(c.get(format!("http://{addr}/nope")).bearer_auth(TOKEN).send().await.unwrap().status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_non_streaming_and_streaming() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    let msg = json!([{"role": "user", "content": "hi"}]);
    let r = c.post(&url).bearer_auth(TOKEN).json(&json!({"messages": msg})).send().await.unwrap();
    assert_eq!(r.headers()["x-weirdnet-model"], "m");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "Hello");
    assert_eq!(v["choices"][0]["message"]["reasoning_content"], "hm");
    assert_eq!(v["usage"]["total_tokens"], 4);

    let text = c.post(&url).bearer_auth(TOKEN).json(&json!({"messages": msg, "stream": true})).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("\"content\":\"Hel\""), "{text}");
    assert!(text.contains("\"reasoning_content\":\"hm\""), "{text}");
    assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
    assert!(text.contains("\"total_tokens\":4"), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");

    let tools = json!([{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]);
    let v: Value = c.post(&url).bearer_auth(TOKEN).json(&json!({"messages": msg, "tools": tools})).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], "{\"q\":1}");
    let text =
        c.post(&url).bearer_auth(TOKEN).json(&json!({"messages": msg, "tools": tools, "stream": true})).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("\"name\":\"lookup\""), "{text}");

    let r = c.post(&url).bearer_auth(TOKEN).json(&json!({"model": "nope", "messages": msg})).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let r = c.post(&url).bearer_auth(TOKEN).json(&json!({"model": "nope", "messages": msg, "stream": true})).send().await.unwrap();
    assert_eq!(r.status(), 404, "a stream that never started is a plain error");
    assert_eq!(c.post(&url).bearer_auth(TOKEN).body("{").send().await.unwrap().status(), 400);
    let img = json!([{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]);
    assert_eq!(c.post(&url).bearer_auth(TOKEN).json(&json!({"messages": img})).send().await.unwrap().status(), 400, "not a data URL");
}

const PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn image_body(model: Option<&str>, images: usize, stream: bool) -> Value {
    let mut parts = vec![json!({"type": "text", "text": "describe"})];
    parts.extend((0..images).map(|_| json!({"type": "image_url", "image_url": {"url": PNG}})));
    let mut b = json!({"messages": [{"role": "user", "content": parts}], "stream": stream});
    if let Some(m) = model {
        b["model"] = m.into();
    }
    b
}

#[tokio::test(flavor = "multi_thread")]
async fn images_reach_a_vision_model_and_never_a_text_only_one() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    for (model, images) in [(Some("vision"), 1), (None, 3)] {
        let r = c.post(&url).bearer_auth(TOKEN).json(&image_body(model, images, false)).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-weirdnet-model"], "vision");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], format!("describe [{images} image(s)]"));
    }
    let text = c.post(&url).bearer_auth(TOKEN).json(&image_body(None, 2, true)).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("[2 image(s)]") && text.trim_end().ends_with("data: [DONE]"), "{text}");

    let r = c.post(&url).bearer_auth(TOKEN).json(&image_body(Some("m"), 1, false)).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("`m` does not take images"), "{v}");

    let audio = json!({"messages": [{"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}}]}]});
    let r = c.post(&url).bearer_auth(TOKEN).json(&audio).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("no model in the pool takes audio"), "{v}");

    let v: Value = c.get(format!("http://{addr}/v1/models")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap();
    assert_eq!((v["data"][1]["id"].as_str(), v["data"][1]["weirdnet"]["vision"].as_bool()), (Some("vision"), Some(true)));
    assert_eq!(v["data"][0]["weirdnet"]["vision"], false);
    let by_id = |id: &str| v["data"].as_array().unwrap().iter().find(|m| m["id"] == id).unwrap()["weirdnet"]["base"].clone();
    assert_eq!((by_id("m-tune"), by_id("m")), (json!("m"), Value::Null));
    let field = |id: &str, f: &str| v["data"].as_array().unwrap().iter().find(|m| m["id"] == id).unwrap()["weirdnet"][f].clone();
    assert_eq!((field("m-tune", "origin"), field("m-tune", "license")), (json!("train"), json!("Apache-2.0")));
    assert_eq!((field("m", "origin"), field("m", "license")), (Value::Null, Value::Null));
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_parameter_and_header_reach_the_node() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    let ask = |body: Value, header: Option<&'static str>| {
        let mut r = c.post(&url).bearer_auth(TOKEN).json(&body);
        if let Some(h) = header {
            r = r.header("x-weirdnet-cache", h);
        }
        r.send()
    };
    let body = |cache: Value| json!({"model": "m", "cache": cache, "messages": [{"role": "user", "content": "hi"}]});
    let content = |v: Value| v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();

    let v: Value = ask(body(json!(null)), None).await.unwrap().json().await.unwrap();
    assert_eq!(content(v), "Hello");
    let v: Value = ask(body(json!(false)), None).await.unwrap().json().await.unwrap();
    assert_eq!(content(v), "cache Bypass");
    let v: Value = ask(body(json!("refresh")), None).await.unwrap().json().await.unwrap();
    assert_eq!(content(v), "cache Refresh");
    let v: Value = ask(body(json!(null)), Some("off")).await.unwrap().json().await.unwrap();
    assert_eq!(content(v), "cache Bypass");
    let v: Value = ask(body(json!("refresh")), Some("off")).await.unwrap().json().await.unwrap();
    assert_eq!(content(v), "cache Bypass", "the stricter of body and header wins");
    let r = ask(body(json!(null)), Some("sometimes")).await.unwrap();
    assert_eq!(r.status(), 400);
    let r = ask(body(json!("sometimes")), None).await.unwrap();
    assert_eq!(r.status(), 400);

    let v: Value = c
        .post(format!("http://{addr}/v1/completions"))
        .bearer_auth(TOKEN)
        .header("x-weirdnet-cache", "refresh")
        .json(&json!({"model": "m", "prompt": "Once"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["choices"][0]["text"], "cache Refresh");
}

#[tokio::test(flavor = "multi_thread")]
async fn standin_cache_and_x_wait_headers() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    let body = json!({"model": "big", "messages": [{"role": "user", "content": "hi"}]});
    let r = c.post(&url).bearer_auth(TOKEN).json(&body).send().await.unwrap();
    assert_eq!(r.headers()["x-weirdnet-standin"], "small");
    assert_eq!(r.headers()["x-weirdnet-model"], "small");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["weirdnet"]["standin"], "small");
    let r = c.post(&url).bearer_auth(TOKEN).header("x-wait", "1").json(&body).send().await.unwrap();
    assert!(r.headers().get("x-weirdnet-standin").is_none());
    assert_eq!(r.headers()["x-weirdnet-model"], "big");

    let mut streaming = body.clone();
    streaming["stream"] = true.into();
    let r = c.post(&url).bearer_auth(TOKEN).json(&streaming).send().await.unwrap();
    assert_eq!(r.headers()["x-weirdnet-standin"], "small");
    let text = r.text().await.unwrap();
    assert!(text.contains("\"standin\":\"small\""), "{text}");

    let cached = json!({"model": "cached", "messages": [{"role": "user", "content": "hi"}]});
    let r = c.post(&url).bearer_auth(TOKEN).json(&cached).send().await.unwrap();
    assert_eq!(r.headers()["x-weirdnet-cache"], "hit");

    // Streamed: the early Meta frame puts the header on the stream too.
    let mut cached_stream = cached.clone();
    cached_stream["stream"] = true.into();
    let r = c.post(&url).bearer_auth(TOKEN).json(&cached_stream).send().await.unwrap();
    assert_eq!(r.headers()["x-weirdnet-cache"], "hit");
    let text = r.text().await.unwrap();
    assert!(text.contains("\"cache_hit\":true") && text.contains("\"content\":\"lo\""), "{text}");

    // A node that reports the hit only at the end: no header, but the final chunk and the
    // metric have it.
    let late = json!({"model": "late-cached", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let r = c.post(&url).bearer_auth(TOKEN).json(&late).send().await.unwrap();
    assert!(r.headers().get("x-weirdnet-cache").is_none());
    assert!(r.text().await.unwrap().contains("\"cache_hit\":true"));

    let m = c.get(format!("http://{addr}/metrics")).bearer_auth(TOKEN).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("weirdnet_bridge_standin_total 2"), "{m}");
    assert!(m.contains("weirdnet_bridge_cache_hits_total 3"), "{m}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_stream_cancels_the_request() {
    let (addr, cancelled) = start().await;
    let c = reqwest::Client::new();
    let body = json!({"model": "slow", "stream": true, "messages": [{"role": "user", "content": "go"}]});
    let mut r = c.post(format!("http://{addr}/v1/chat/completions")).bearer_auth(TOKEN).json(&body).send().await.unwrap();
    let first = r.chunk().await.unwrap().unwrap();
    assert!(!first.is_empty());
    drop(r);
    for _ in 0..100 {
        if *cancelled.lock().unwrap() == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the request was not cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn embeddings_with_usage() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let v: Value = c
        .post(format!("http://{addr}/v1/embeddings"))
        .bearer_auth(TOKEN)
        .json(&json!({"input": ["ab", "abcd"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["object"], "list");
    assert_eq!(v["data"][1]["index"], 1);
    assert_eq!(v["data"][1]["embedding"][0], 4.0);
    assert_eq!(v["usage"], json!({"prompt_tokens": 6, "total_tokens": 6}));
    // A node's bad request stays a 400 through the pool (it used to become a 503).
    let r =
        c.post(format!("http://{addr}/v1/embeddings")).bearer_auth(TOKEN).json(&json!({"input": "x", "model": "chat-only"})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "bad_request");
    let r = c
        .post(format!("http://{addr}/v1/embeddings"))
        .bearer_auth(TOKEN)
        .json(&json!({"input": "x", "encoding_format": "base64"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn text_completions_whole_and_streamed() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/completions");
    let r = c.post(&url).bearer_auth(TOKEN).json(&json!({"prompt": "Once", "max_tokens": 3})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-weirdnet-model"], "m");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["object"], "text_completion");
    assert!(v["id"].as_str().unwrap().starts_with("cmpl-"));
    assert_eq!(v["choices"][0]["text"], " upon a time");
    assert_eq!(v["choices"][0]["finish_reason"], "length");
    assert_eq!(v["usage"]["total_tokens"], 4);

    let r = c.post(&url).bearer_auth(TOKEN).json(&json!({"prompt": ["Once"], "stream": true})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    let chunks: Vec<Value> =
        text.lines().filter_map(|l| l.strip_prefix("data: ")).filter(|d| *d != "[DONE]").map(|d| serde_json::from_str(d).unwrap()).collect();
    let joined: String = chunks.iter().filter_map(|c| c["choices"][0]["text"].as_str()).collect();
    assert_eq!(joined, " upon a time");
    assert!(chunks.iter().all(|c| c["object"] == "text_completion"));
    assert_eq!(chunks.last().unwrap()["choices"][0]["finish_reason"], "length");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");

    let r = c.post(&url).bearer_auth(TOKEN).json(&json!({"prompt": "Once", "model": "chat-only"})).send().await.unwrap();
    assert_eq!(r.status(), 501);
    let r = c.post(&url).bearer_auth(TOKEN).json(&json!({"prompt": "Once", "n": 2})).send().await.unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn continue_final_message_returns_only_the_continuation() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    let msgs = json!([{"role": "user", "content": "hi"}, {"role": "assistant", "content": "Hel"}]);
    let body = json!({"messages": msgs, "continue_final_message": true, "add_generation_prompt": false});
    let v: Value = c.post(&url).bearer_auth(TOKEN).json(&body).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "lo there");

    let mut streamed = body.clone();
    streamed["stream"] = true.into();
    let text = c.post(&url).bearer_auth(TOKEN).json(&streamed).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("\"content\":\"lo there\""), "{text}");
    assert!(!text.contains("\"content\":\"Hel"), "{text}");

    // A node that cannot continue: 501, as for anything unsupported.
    let mut chat_only = body.clone();
    chat_only["model"] = "chat-only".into();
    assert_eq!(c.post(&url).bearer_auth(TOKEN).json(&chat_only).send().await.unwrap().status(), 501);
    let mut bad = body;
    bad["messages"] = json!([{"role": "user", "content": "hi"}]);
    assert_eq!(c.post(&url).bearer_auth(TOKEN).json(&bad).send().await.unwrap().status(), 400);
}

/// A chat body of exactly `len` bytes.
fn chat_body(len: usize) -> String {
    let make = |pad: usize| json!({"model": "m", "messages": [{"role": "user", "content": "a".repeat(pad)}]}).to_string();
    let base = make(0).len();
    assert!(len >= base);
    let body = make(len - base);
    assert_eq!(body.len(), len);
    body
}

async fn assert_too_large(r: reqwest::Response, limit: usize, human: &str) {
    assert_eq!(r.status(), 413);
    assert_eq!(r.headers()["x-weirdnet-max-request-bytes"], limit.to_string().as_str());
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "request_too_large");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains(&format!("{limit} bytes")) && msg.contains(human) && msg.contains("max_request_bytes"), "{msg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_default_request_limit_is_32_mib() {
    assert_eq!(DEFAULT_MAX_REQUEST_BYTES, 32 << 20);
    assert_eq!(Bridge::bind(BridgeOptions::new(0, TOKEN)).await.unwrap().with_max_request_bytes(0).max_request_bytes(), 1, "at least one byte");
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    let r = c.post(&url).bearer_auth(TOKEN).header("content-type", "application/json").body(chat_body(1 << 20)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = c.post(&url).bearer_auth(TOKEN).header("content-type", "application/json").body(chat_body((32 << 20) + 1)).send().await.unwrap();
    assert_too_large(r, 32 << 20, "32 MiB").await;
    let v: Value = c.get(format!("http://{addr}/v1/models")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"][0]["weirdnet"]["max_request_bytes"], 32 << 20);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_custom_request_limit_admits_up_to_it_and_names_it_in_the_413() {
    let limit = 64 << 10;
    let (addr, _) = start_with(Some(limit)).await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    let post = |body: String| c.post(&url).bearer_auth(TOKEN).header("content-type", "application/json").body(body);
    let r = post(chat_body(limit)).send().await.unwrap();
    assert_eq!(r.status(), 200, "exactly at the limit");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "Hello");
    assert_too_large(post(chat_body(limit + 1)).send().await.unwrap(), limit, "64 KiB").await;
    // A chunked body with no Content-Length is cut off at the limit too.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = chat_body(limit + 4096).into_bytes().chunks(4096).map(|c| Ok(c.to_vec())).collect();
    let r = c
        .post(&url)
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .unwrap();
    assert_too_large(r, limit, "64 KiB").await;
    let r = c.post(format!("http://{addr}/v1/embeddings")).bearer_auth(TOKEN).body(vec![b' '; limit + 1]).send().await.unwrap();
    assert_too_large(r, limit, "64 KiB").await;
    // Unauthenticated callers learn nothing about it.
    assert_eq!(c.post(&url).body(chat_body(limit + 1)).send().await.unwrap().status(), 401);

    // /v1/models reports the limit per model: the smaller of the bridge's and the node's, plus
    // the media limits the node knows.
    let v: Value = c.get(format!("http://{addr}/v1/models")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap();
    let vision = v["data"].as_array().unwrap().iter().find(|m| m["id"] == "vision").unwrap();
    assert_eq!(vision["weirdnet"]["max_request_bytes"], limit);
    assert_eq!(vision["weirdnet"]["limits"], json!({"max_images": 4, "image_tokens": 256, "image_edge": 896, "max_request_bytes": limit}));
    assert_eq!(v["data"][0]["weirdnet"]["limits"], json!({"max_request_bytes": limit}), "a model without limits gets only the bridge's");
}
