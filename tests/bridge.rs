//! The bridge over a real `weirdnet-agent` pool on memory channels, with a fake inference node.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use weirdnet_agent::remote::catalog::{self, CatalogFeed, CatalogModel, ModelState, Purpose};
use weirdnet_agent::remote::infer::v2::{
    self, AudioHeader, AudioStream, CacheMode, EmbedBackend, Finish, InferBackend, Meta, Output, Request, Segment, SpeakBackend, SpeakInput,
    SpeakRequest, SpeechPiece, TranscribeBackend, TranscribeRequest, Transcript, Usage, Voice,
};
use weirdnet_agent::remote::infer::{Admission, InferHost};
use weirdnet_agent::remote::pool::Pool;
use weirdnet_agent::remote::{Caller, MemoryDialer, RemoteError};
use weirdnet_agent::{AudioFormat, CancelToken, CloneMode, ContentPart, MediaLimits, Modalities, ModelBackend, SpeechProfile, ToolCall};
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
        if self.model == "video" {
            let m = &request.inline_messages().unwrap()[0];
            let clips = m.content.parts().iter().filter(|p| matches!(p, ContentPart::VideoUrl { .. })).count();
            out.text(format!("{} [{clips} clip(s)]", m.text()));
            return Ok(Finish::new("stop").with_usage(Usage::new(5, 3)));
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
            Some("video") => "video",
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
    fn open_speak(&self, _caller: &Caller, request: &SpeakRequest) -> Result<Arc<dyn SpeakBackend>, RemoteError> {
        match request.model.as_deref() {
            Some("tts") => Ok(Arc::new(Speaker)),
            other => Err(RemoteError::unavailable(format!("unknown model `{}`", other.unwrap_or_default()))),
        }
    }
    fn open_transcribe(&self, _caller: &Caller, model: Option<&str>) -> Result<Arc<dyn TranscribeBackend>, RemoteError> {
        match model {
            Some(m @ ("stt" | "stt-untimed")) => Ok(Arc::new(Transcriber { model: m.to_string() })),
            other => Err(RemoteError::unavailable(format!("unknown model `{}`", other.unwrap_or_default()))),
        }
    }
}

/// Speaks `voice|text|format|speed` as the "audio" (24 kHz mono), in three pieces when streamed;
/// the text "fail midway" fails after the first piece. A cloned voice's reference transcript is
/// reported as ignored (as VoxCPM2 does).
struct Speaker;

#[async_trait]
impl SpeakBackend for Speaker {
    fn name(&self) -> String {
        "tts".into()
    }
    async fn speak(&self, request: &SpeakRequest, _cancel: &CancelToken) -> Result<AudioStream, RemoteError> {
        let voice = match &request.voice {
            None => "default".to_string(),
            Some(Voice::Preset { name }) => name.clone(),
            Some(Voice::Design { description }) => format!("design:{description}"),
            Some(Voice::Clone { audio, format, .. }) => format!("clone:{format}:{}", audio.len()),
            Some(_) => "other".to_string(),
        };
        let text = match &request.input {
            SpeakInput::Text(t) => t.clone(),
            SpeakInput::Script(turns) => turns.iter().map(|t| format!("S{}:{}", t.speaker, t.text)).collect::<Vec<_>>().join("/"),
            SpeakInput::Raw(t) => format!("raw:{t}"),
            _ => String::new(),
        };
        let format = request.format.unwrap_or(AudioFormat::Wav);
        let header = AudioHeader::new(format).with_sample_rate(24_000).with_channels(1);
        let mut pieces = vec![Ok(SpeechPiece::Start(header))];
        if text == "fail midway" {
            pieces.push(Ok(SpeechPiece::Audio(b"partial".to_vec())));
            pieces.push(Err(RemoteError::Failed { message: "the engine stopped".into() }));
            return Ok(AudioStream::from_pieces(pieces));
        }
        let body = format!("{voice}|{text}|{format}|{}", request.speed.unwrap_or(1000)).into_bytes();
        if request.stream {
            let third = body.len().div_ceil(3);
            pieces.extend(body.chunks(third).map(|c| Ok(SpeechPiece::Audio(c.to_vec()))));
        } else {
            pieces.push(Ok(SpeechPiece::Audio(body)));
        }
        if matches!(&request.voice, Some(Voice::Clone { text: Some(_), .. })) {
            pieces.push(Ok(SpeechPiece::Meta(Meta::default().with_ignored("reference_text"))));
        }
        pieces.push(Ok(SpeechPiece::Usage(Usage::speech(text.chars().count() as u64, 1000))));
        Ok(AudioStream::from_pieces(pieces))
    }
}

/// "stt" answers `heard <bytes> bytes of <format>` (with the language and prompt when given)
/// and, when asked, two segments; "stt-untimed" never times anything.
struct Transcriber {
    model: String,
}

#[async_trait]
impl TranscribeBackend for Transcriber {
    fn name(&self) -> String {
        self.model.clone()
    }
    async fn transcribe(&self, request: &TranscribeRequest, _cancel: &CancelToken) -> Result<Transcript, RemoteError> {
        let mut text = format!("heard {} bytes of {}", request.audio.len(), request.format);
        if let Some(l) = &request.language {
            text.push_str(&format!(" in {l}"));
        }
        if let Some(p) = &request.prompt {
            text.push_str(&format!(" after {p}"));
        }
        let t = Transcript::new(text);
        if self.model == "stt-untimed" {
            return Ok(t);
        }
        let t = t.with_language("en").with_duration_ms(2500);
        Ok(if request.timestamps { t.with_segments(vec![Segment::new(0, 1200, "Hello"), Segment::new(1200, 2500, "world.")]) } else { t })
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
        CatalogModel::new("video", Purpose::Chat, ModelState::Loadable)
            .with_inputs(Modalities::VIDEO)
            .with_limits(MediaLimits::new().with_video_max_seconds(60).with_video_max_bytes(16 << 20).with_video_frames_per_minute(120)),
        CatalogModel::new("tts", Purpose::Speak, ModelState::Warm).with_replicas(1).with_speech(
            SpeechProfile::new()
                .with_presets(vec!["vivian".into(), "ryan".into()])
                .with_formats(vec![AudioFormat::Wav, AudioFormat::Pcm16])
                .with_sample_rate(24_000)
                .with_design(true)
                .with_clone(CloneMode::Audio)
                .with_script_speakers(2)
                .with_speed(true)
                .with_streaming(true)
                .with_max_input_chars(2000),
        ),
        CatalogModel::new("stt", Purpose::Transcribe, ModelState::Warm).with_replicas(1),
        CatalogModel::new("stt-untimed", Purpose::Transcribe, ModelState::Loadable),
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

const MP4: &str = "data:video/mp4;base64,AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXZjMW1wNDE=";
/// A 44-byte WAV header with no samples, base64.
const WAV_B64: &str = "UklGRiQAAABXQVZFZm10IBAAAAABAAEAwF0AAIC7AAACABAAZGF0YQAAAAA=";

fn video_body(model: Option<&str>, url: &str) -> Value {
    let parts = json!([{"type": "text", "text": "watch"}, {"type": "video_url", "video_url": {"url": url}}]);
    let mut b = json!({"messages": [{"role": "user", "content": parts}]});
    if let Some(m) = model {
        b["model"] = m.into();
    }
    b
}

#[tokio::test(flavor = "multi_thread")]
async fn video_clips_reach_a_video_model_and_remote_urls_are_refused() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    for model in [Some("video"), None] {
        let r = c.post(&url).bearer_auth(TOKEN).json(&video_body(model, MP4)).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-weirdnet-model"], "video");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "watch [1 clip(s)]");
    }
    // A bare string URL is taken too.
    let bare = json!({"messages": [{"role": "user", "content": [{"type": "video_url", "video_url": MP4}]}]});
    let r = c.post(&url).bearer_auth(TOKEN).json(&bare).send().await.unwrap();
    assert_eq!(r.status(), 200);

    for remote in ["https://example.com/clip.mp4", "file:///etc/passwd", "data:image/png;base64,AAAA"] {
        let r = c.post(&url).bearer_auth(TOKEN).json(&video_body(Some("video"), remote)).send().await.unwrap();
        assert_eq!(r.status(), 400, "{remote}");
        let v: Value = r.json().await.unwrap();
        assert!(v["error"]["message"].as_str().unwrap().contains("data:video/"), "{v}");
    }
    let r = c.post(&url).bearer_auth(TOKEN).json(&video_body(Some("m"), MP4)).send().await.unwrap();
    assert_eq!(r.status(), 400, "a text-only model never gets video");
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("`m`"), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn models_list_purpose_inputs_speech_and_video_limits() {
    let (addr, _) = start().await;
    let c = reqwest::Client::new();
    let v: Value = c.get(format!("http://{addr}/v1/models")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap();
    let w = |id: &str| v["data"].as_array().unwrap().iter().find(|m| m["id"] == id).unwrap()["weirdnet"].clone();
    let video = w("video");
    assert_eq!(video["video"], true);
    assert_eq!(video["inputs"], json!({"vision": false, "audio": false, "video": true}));
    assert_eq!(video["limits"]["video_max_seconds"], 60);
    assert_eq!(video["limits"]["video_max_bytes"], 16 << 20);
    assert_eq!(video["limits"]["video_frames_per_minute"], 120);
    assert_eq!(video["purpose"], "chat");
    assert!(video.get("speech").is_none(), "no profile, no field");
    assert_eq!(w("vision")["inputs"], json!({"vision": true, "audio": false, "video": false}));

    let tts = w("tts");
    assert_eq!(tts["purpose"], "speak");
    assert_eq!(tts["speech"]["presets"], json!(["vivian", "ryan"]));
    assert_eq!(tts["speech"]["formats"], json!(["wav", "pcm16"]));
    assert_eq!(tts["speech"]["sample_rate"], 24_000);
    assert_eq!(tts["max_request_bytes"], DEFAULT_MAX_REQUEST_BYTES);
    assert_eq!(w("stt")["purpose"], "transcribe");
}

async fn speak(addr: std::net::SocketAddr, body: Value) -> reqwest::Response {
    reqwest::Client::new().post(format!("http://{addr}/v1/audio/speech")).bearer_auth(TOKEN).json(&body).send().await.unwrap()
}

async fn error_message(r: reqwest::Response) -> String {
    let v: Value = r.json().await.unwrap();
    v["error"]["message"].as_str().unwrap_or_default().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn speech_whole_with_the_real_format_and_rate() {
    let (addr, _) = start().await;
    // The OpenAI SDK's shape.
    let r = speak(addr, json!({"model": "tts", "input": "Hello there", "voice": "vivian", "speed": 1.25})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "audio/wav");
    assert_eq!(r.headers()["x-audio-sample-rate"], "24000");
    assert_eq!(r.headers()["x-weirdnet-audio-channels"], "1");
    assert_eq!(r.headers()["x-weirdnet-model"], "tts");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"vivian|Hello there|wav|1250");

    // Raw PCM says its rate in the content type; `{"id": ...}` voices and no model work too.
    let r = speak(addr, json!({"input": "Hi", "voice": {"id": "ryan"}, "response_format": "pcm"})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "audio/pcm;rate=24000");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"ryan|Hi|pcm16|1000");

    // No voice: the model's default.
    let r = speak(addr, json!({"model": "tts", "input": "Hi"})).await;
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"default|Hi|wav|1000");

    // Weirdnet extensions: a designed voice, a script, a cloned voice from a data: URL.
    let r = speak(addr, json!({"model": "tts", "input": "Hi", "weirdnet": {"design": "a calm narrator", "seed": 7, "language": "en"}})).await;
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"design:a calm narrator|Hi|wav|1000");
    let script = json!([{"speaker": 0, "text": "Hi"}, {"speaker": 1, "text": "Hello"}]);
    let r = speak(addr, json!({"model": "tts", "weirdnet": {"script": script}})).await;
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"default|S0:Hi/S1:Hello|wav|1000");
    let clone = json!({"ref_audio": format!("data:audio/wav;base64,{WAV_B64}"), "ref_text": "what it says", "consent": true});
    let r = speak(addr, json!({"model": "tts", "input": "Hi", "weirdnet": clone})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-weirdnet-ignored"], "reference_text");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"clone:wav:44|Hi|wav|1000");
}

#[tokio::test(flavor = "multi_thread")]
async fn speech_streams_as_it_comes() {
    let (addr, _) = start().await;
    let r = speak(addr, json!({"model": "tts", "input": "Hello there", "voice": "ryan", "stream_format": "audio"})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "audio/wav");
    assert_eq!(r.headers()["x-audio-sample-rate"], "24000");
    assert!(r.headers().get("content-length").is_none(), "streamed");
    let mut stream = r.bytes_stream();
    let mut got = Vec::new();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(got, b"ryan|Hello there|wav|1000");

    // A failure after audio left ends the body early; nothing is spliced in from another node.
    // (The failure here comes right after the first piece, so the connection may close before the
    // headers are flushed; either way the client never sees a complete answer.)
    let sent = reqwest::Client::new()
        .post(format!("http://{addr}/v1/audio/speech"))
        .bearer_auth(TOKEN)
        .json(&json!({"model": "tts", "input": "fail midway", "stream_format": "audio"}))
        .send()
        .await;
    match sent {
        Ok(r) => {
            assert_eq!(r.status(), 200);
            assert!(r.bytes().await.is_err(), "the body is cut, not completed");
        }
        Err(e) => assert!(e.is_request(), "{e}"),
    }
    // Without streaming the same failure is a proper error.
    let r = speak(addr, json!({"model": "tts", "input": "fail midway"})).await;
    assert_eq!(r.status(), 502);

    let r = speak(addr, json!({"model": "tts", "input": "Hi", "stream_format": "sse"})).await;
    assert_eq!(r.status(), 400);
    assert!(error_message(r).await.contains("\"sse\" is not supported"));
}

#[tokio::test(flavor = "multi_thread")]
async fn speech_refuses_unknown_voices_with_the_presets_and_never_remaps() {
    let (addr, _) = start().await;
    for (body, needle) in [
        (json!({"model": "tts", "input": "Hi", "voice": "alloy"}), "has no preset voice `alloy` (presets: vivian, ryan)"),
        (json!({"input": "Hi", "voice": "alloy"}), "has no preset voice `alloy` (presets: vivian, ryan)"),
        (json!({"model": "tts", "input": "Hi", "response_format": "mp3"}), "does not produce `mp3` (formats: wav, pcm16)"),
        (json!({"model": "tts", "input": "Hi", "response_format": "aac"}), "use one of wav, pcm, mp3, opus or flac"),
        (json!({"model": "tts", "input": "Hi", "instructions": "whisper"}), "does not take style instructions"),
        (json!({"model": "tts", "weirdnet": {"script": [{"speaker": 2, "text": "Hi"}]}}), "at most 2 speakers"),
        (json!({"model": "m", "input": "Hi"}), "model `m` does not speak"),
        (json!({"model": "tts", "input": "Hi", "voice": "vivian", "weirdnet": {"design": "calm"}}), "choose one voice"),
        (json!({"model": "tts", "input": "  "}), "needs text to speak"),
        (json!({"model": "tts"}), "`input` (the text to speak) is required"),
        (json!({"model": "tts", "input": "Hi", "speed": 0}), "`speed` must be a positive number"),
    ] {
        let r = speak(addr, body.clone()).await;
        assert_eq!(r.status(), 400, "{body}");
        let m = error_message(r).await;
        assert!(m.contains(needle), "{body}: {m}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn speech_clones_only_from_data_urls_with_consent() {
    let (addr, _) = start().await;
    for (weirdnet, needle) in [
        (json!({"ref_audio": "https://example.com/voice.wav", "consent": true}), "must be a data: URL"),
        (json!({"ref_audio": "file:///home/voice.wav", "consent": true}), "must be a data: URL"),
        (json!({"ref_audio": "data:audio/wav,raw", "consent": true}), "must be a base64 data: URL"),
        (json!({"ref_audio": "data:audio/wav;base64,!!!", "consent": true}), "invalid base64"),
        (json!({"ref_audio": format!("data:audio/wav;base64,{WAV_B64}")}), "needs consent"),
        (json!({"ref_audio": format!("data:audio/wav;base64,{WAV_B64}"), "consent": false}), "needs consent"),
        (json!({"ref_text": "hello", "consent": true}), "go with `weirdnet.ref_audio`"),
    ] {
        let r = speak(addr, json!({"model": "tts", "input": "Hi", "weirdnet": weirdnet})).await;
        assert_eq!(r.status(), 400, "{weirdnet}");
        let m = error_message(r).await;
        assert!(m.contains(needle), "{weirdnet}: {m}");
    }
}

fn wav_part(len: usize) -> reqwest::multipart::Part {
    let mut bytes = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    bytes.resize(len.max(16), 0);
    reqwest::multipart::Part::bytes(bytes).file_name("clip.wav").mime_str("audio/wav").unwrap()
}

async fn transcribe(addr: std::net::SocketAddr, form: reqwest::multipart::Form) -> reqwest::Response {
    reqwest::Client::new().post(format!("http://{addr}/v1/audio/transcriptions")).bearer_auth(TOKEN).multipart(form).send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn transcriptions_in_every_response_format() {
    use reqwest::multipart::Form;
    let (addr, _) = start().await;

    // json (the default), with the OpenAI SDK's fields.
    let form = Form::new().part("file", wav_part(100)).text("model", "stt").text("language", "fr").text("prompt", "Bonjour").text("temperature", "0");
    let r = transcribe(addr, form).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-weirdnet-model"], "stt");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v, json!({"text": "heard 100 bytes of wav in fr after Bonjour"}));

    // No model: the catalog's transcriber.
    let r = transcribe(addr, Form::new().part("file", wav_part(50))).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["text"], "heard 50 bytes of wav");

    let r = transcribe(addr, Form::new().part("file", wav_part(50)).text("model", "stt").text("response_format", "text")).await;
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/plain"));
    assert_eq!(r.text().await.unwrap(), "heard 50 bytes of wav\n");

    let form = Form::new()
        .part("file", wav_part(50))
        .text("model", "stt")
        .text("response_format", "verbose_json")
        .text("timestamp_granularities[]", "segment");
    let v: Value = transcribe(addr, form).await.json().await.unwrap();
    assert_eq!(v["task"], "transcribe");
    assert_eq!(v["language"], "en");
    assert_eq!(v["duration"], 2.5);
    assert_eq!(v["segments"], json!([{"id": 0, "start": 0.0, "end": 1.2, "text": "Hello"}, {"id": 1, "start": 1.2, "end": 2.5, "text": "world."}]));

    let r = transcribe(addr, Form::new().part("file", wav_part(50)).text("model", "stt").text("response_format", "srt")).await;
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("application/x-subrip"));
    assert_eq!(r.text().await.unwrap(), "1\n00:00:00,000 --> 00:00:01,200\nHello\n\n2\n00:00:01,200 --> 00:00:02,500\nworld.\n\n");

    let r = transcribe(addr, Form::new().part("file", wav_part(50)).text("model", "stt").text("response_format", "vtt")).await;
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/vtt"));
    assert_eq!(r.text().await.unwrap(), "WEBVTT\n\n00:00:00.000 --> 00:00:01.200\nHello\n\n00:00:01.200 --> 00:00:02.500\nworld.\n\n");

    // A transcriber that times nothing can't make subtitles, and says so rather than inventing cues.
    let r = transcribe(addr, Form::new().part("file", wav_part(50)).text("model", "stt-untimed").text("response_format", "srt")).await;
    assert_eq!(r.status(), 501);
    assert!(error_message(r).await.contains("no timed segments"));

    // The format comes from the bytes when the name and type don't say.
    let anon = reqwest::multipart::Part::bytes(b"fLaC\0\0\0\0".to_vec()).file_name("upload");
    let r = transcribe(addr, Form::new().part("file", anon).text("model", "stt")).await;
    assert_eq!(r.json::<Value>().await.unwrap()["text"], "heard 8 bytes of flac");
}

#[tokio::test(flavor = "multi_thread")]
async fn transcription_refusals() {
    use reqwest::multipart::Form;
    let (addr, _) = start().await;
    let text_file = || reqwest::multipart::Part::bytes(b"hello".to_vec()).file_name("notes.txt").mime_str("text/plain").unwrap();
    for (form, status, needle) in [
        (Form::new().text("model", "stt"), 400, "`file` (the audio to transcribe) is required"),
        (Form::new().part("file", text_file()).text("model", "stt"), 400, "could not tell the audio's format"),
        (Form::new().part("file", wav_part(50)).text("response_format", "tsv"), 400, "not one of json, verbose_json, text, srt or vtt"),
        (Form::new().part("file", wav_part(50)).text("timestamp_granularities[]", "word"), 400, "`word` is not supported"),
        (Form::new().part("file", wav_part(50)).text("stream", "true"), 400, "streamed transcription is not supported"),
        (Form::new().part("file", wav_part(50)).text("model", "m"), 400, "`m`"),
    ] {
        let r = transcribe(addr, form).await;
        assert_eq!(r.status(), status);
        let m = error_message(r).await;
        assert!(m.contains(needle), "{m}");
    }
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/audio/transcriptions"))
        .bearer_auth(TOKEN)
        .json(&json!({"file": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(error_message(r).await.contains("multipart/form-data"));
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_endpoints_get_the_413_that_names_the_limit() {
    use reqwest::multipart::Form;
    let limit = 64 << 10;
    let (addr, _) = start_with(Some(limit)).await;
    let r = speak(addr, json!({"model": "tts", "input": "x".repeat(limit)})).await;
    assert_too_large(r, limit, "64 KiB").await;
    let clone = json!({"ref_audio": format!("data:audio/wav;base64,{}", "A".repeat(limit)), "consent": true});
    let r = speak(addr, json!({"model": "tts", "input": "Hi", "weirdnet": clone})).await;
    assert_too_large(r, limit, "64 KiB").await;
    let r = transcribe(addr, Form::new().part("file", wav_part(limit + 1)).text("model", "stt")).await;
    assert_too_large(r, limit, "64 KiB").await;
    // Just under the limit is fine.
    let r = transcribe(addr, Form::new().part("file", wav_part(limit - 1024)).text("model", "stt")).await;
    assert_eq!(r.status(), 200);
}
