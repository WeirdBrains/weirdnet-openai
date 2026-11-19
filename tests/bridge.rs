//! The bridge over a real `weirdnet-agent` pool on memory channels, with a fake inference node.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use weirdnet_agent::remote::catalog::{self, CatalogFeed, CatalogModel, ModelState, Purpose};
use weirdnet_agent::remote::infer::v2::{self, EmbedBackend, Finish, InferBackend, Output, Request, Usage};
use weirdnet_agent::remote::infer::{Admission, InferHost};
use weirdnet_agent::remote::pool::Pool;
use weirdnet_agent::remote::{Caller, MemoryDialer, RemoteError};
use weirdnet_agent::{CancelToken, ModelBackend, ToolCall};
use weirdnet_openai::{Bridge, BridgeOptions, new_token};

const TOKEN: &str = "0123456789abcdef0123";

/// "big" is cold: a stand-in ("small") answers unless the request waits; "slow" streams until
/// cancelled; "cached" is a cache hit; with tools, it calls the first one.
struct Fake {
    model: String,
    cancelled: Arc<Mutex<u32>>,
}

#[async_trait]
impl InferBackend for Fake {
    fn name(&self) -> String {
        self.model.clone()
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
        f.meta.cache_hit = self.model == "cached";
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
            Some(other) => return Err(RemoteError::unavailable(format!("unknown model `{other}`"))),
        };
        Ok(Arc::new(Fake { model: model.into(), cancelled: self.cancelled.clone() }))
    }
    fn open_embed(&self, _caller: &Caller, model: Option<&str>) -> Result<Arc<dyn EmbedBackend>, RemoteError> {
        Ok(Arc::new(Fake { model: model.unwrap_or("e").into(), cancelled: self.cancelled.clone() }))
    }
}

async fn start() -> (std::net::SocketAddr, Arc<Mutex<u32>>) {
    let cancelled = Arc::new(Mutex::new(0));
    let host: Arc<dyn InferHost> = Arc::new(Host { cancelled: cancelled.clone() });
    let feed = CatalogFeed::new(Arc::new(|_: &Caller| Ok(())));
    feed.publish(vec![CatalogModel::new("m", Purpose::Chat, ModelState::Warm).with_replicas(1)]);
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
    let bridge = Bridge::bind(BridgeOptions::new(0, TOKEN)).await.unwrap();
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
    assert_eq!(c.post(&url).bearer_auth(TOKEN).json(&json!({"messages": img})).send().await.unwrap().status(), 400);
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

    let m = c.get(format!("http://{addr}/metrics")).bearer_auth(TOKEN).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("weirdnet_bridge_standin_total 2"), "{m}");
    assert!(m.contains("weirdnet_bridge_cache_hits_total 1"), "{m}");
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
async fn embeddings_and_no_completions() {
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
    let r = c
        .post(format!("http://{addr}/v1/embeddings"))
        .bearer_auth(TOKEN)
        .json(&json!({"input": "x", "encoding_format": "base64"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = c.post(format!("http://{addr}/v1/completions")).bearer_auth(TOKEN).json(&json!({"prompt": "Once"})).send().await.unwrap();
    assert_eq!(r.status(), 501);
}
