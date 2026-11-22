//! OpenAI request bodies to `weirdnet-agent` infer/2 requests.

use serde_json::{Map, Value};
use weirdnet_agent::remote::infer::v2::{CacheMode, EmbedRequest, Request, ResponseFormat};
use weirdnet_agent::{Content, ContentPart, Message, Role, ToolCall, ToolSpec};

/// A request the bridge cannot take; answered with 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRequest(pub String);

fn bad(msg: impl Into<String>) -> BadRequest {
    BadRequest(msg.into())
}

fn object(body: Value) -> Result<Map<String, Value>, BadRequest> {
    match body {
        Value::Object(o) => Ok(o),
        _ => Err(bad("body must be a JSON object")),
    }
}

/// One content part: `text`, `image_url` (a `data:image/...` URL, as an object or a bare string)
/// or `input_audio` (`data` and `format`). Other types are refused.
fn part(p: &Value) -> Result<ContentPart, BadRequest> {
    match p["type"].as_str() {
        Some("text") => Ok(ContentPart::text(p["text"].as_str().unwrap_or_default())),
        Some("image_url") => {
            let (url, detail) = match &p["image_url"] {
                Value::String(u) => (u.as_str(), None),
                o => (o["url"].as_str().ok_or_else(|| bad("image_url parts need `image_url.url`"))?, o["detail"].as_str()),
            };
            let mut part = ContentPart::image(url);
            if let (ContentPart::ImageUrl { image_url }, Some(d)) = (&mut part, detail) {
                image_url.detail = Some(d.to_string());
            }
            Ok(part)
        }
        Some("input_audio") => {
            let a = &p["input_audio"];
            Ok(ContentPart::audio(a["data"].as_str().unwrap_or_default(), a["format"].as_str().unwrap_or_default()))
        }
        Some(other) => Err(bad(format!("content parts of type `{other}` are not supported"))),
        None => Err(bad("content parts need a `type`")),
    }
}

/// Message content: a string, or parts. Text-only parts are joined into a string (the form every
/// node reads); images and audio go through as parts, checked by [`Content::check`].
fn content(v: Option<&Value>) -> Result<Content, BadRequest> {
    match v {
        None | Some(Value::Null) => Ok(Content::default()),
        Some(Value::String(s)) => Ok(s.clone().into()),
        Some(Value::Array(parts)) => {
            let c = Content::from(parts.iter().map(part).collect::<Result<Vec<_>, _>>()?).normalized();
            c.check().map_err(bad)?;
            Ok(c)
        }
        Some(_) => Err(bad("`content` must be a string or an array of parts")),
    }
}

fn tool_calls(v: Option<&Value>) -> Result<Vec<ToolCall>, BadRequest> {
    let Some(Value::Array(calls)) = v else { return Ok(Vec::new()) };
    calls
        .iter()
        .map(|c| {
            let f = &c["function"];
            let name = f["name"].as_str().ok_or_else(|| bad("tool calls need `function.name`"))?;
            let arguments = match &f["arguments"] {
                Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())),
                Value::Null => Value::Object(Map::new()),
                other => other.clone(),
            };
            Ok(ToolCall { id: c["id"].as_str().unwrap_or_default().to_string(), name: name.to_string(), arguments })
        })
        .collect()
}

fn message(m: &Value) -> Result<Message, BadRequest> {
    let text = content(m.get("content"))?;
    match m["role"].as_str() {
        Some("system" | "developer") => Ok(Message::system(text)),
        Some("user") => Ok(Message::user(text)),
        Some("assistant") => Ok(Message::assistant(text, tool_calls(m.get("tool_calls"))?)),
        Some("tool") => Ok(Message::tool(m["tool_call_id"].as_str().ok_or_else(|| bad("tool messages need `tool_call_id`"))?, text)),
        Some(other) => Err(bad(format!("unknown role `{other}`"))),
        None => Err(bad("messages need a `role`")),
    }
}

fn tools(v: Option<&Value>) -> Result<Vec<ToolSpec>, BadRequest> {
    let Some(Value::Array(tools)) = v else { return Ok(Vec::new()) };
    tools
        .iter()
        .map(|t| {
            if t["type"].as_str().is_some_and(|k| k != "function") {
                return Err(bad("only function tools are supported"));
            }
            let f = &t["function"];
            let name = f["name"].as_str().ok_or_else(|| bad("tools need `function.name`"))?;
            let mut spec = ToolSpec::new(name, f["description"].as_str().unwrap_or_default());
            if !f["parameters"].is_null() {
                spec.input_schema = f["parameters"].clone();
            }
            Ok(spec)
        })
        .collect()
}

fn response_format(v: Option<&Value>) -> Result<Option<ResponseFormat>, BadRequest> {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(None) };
    match v["type"].as_str() {
        Some("text") => Ok(Some(ResponseFormat::Text)),
        Some("json_object") => Ok(Some(ResponseFormat::Json)),
        Some("json_schema") => {
            let s = &v["json_schema"];
            Ok(Some(ResponseFormat::JsonSchema {
                name: s["name"].as_str().unwrap_or("response").to_string(),
                schema: s["schema"].clone(),
                strict: s["strict"].as_bool().unwrap_or(false),
            }))
        }
        _ => Err(bad("`response_format.type` must be text, json_object or json_schema")),
    }
}

fn f32_of(o: &Map<String, Value>, key: &str) -> Result<Option<f32>, BadRequest> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().map(|f| Some(f as f32)).ok_or_else(|| bad(format!("`{key}` must be a number"))),
    }
}

fn u32_of(o: &Map<String, Value>, key: &str) -> Result<Option<u32>, BadRequest> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().and_then(|n| u32::try_from(n).ok()).map(Some).ok_or_else(|| bad(format!("`{key}` must be a whole number"))),
    }
}

/// `model`, if given, must be a string.
fn model(o: &Map<String, Value>) -> Result<Option<String>, BadRequest> {
    match o.get("model") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(m)) if !m.is_empty() => Ok(Some(m.clone())),
        _ => Err(bad("`model` must be a non-empty string")),
    }
}

/// A boolean parameter, false when absent.
fn flag(o: &Map<String, Value>, key: &str) -> Result<bool, BadRequest> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad(format!("`{key}` must be true or false"))),
    }
}

/// A `/v1/chat/completions` body: the request and whether to stream. Known sampling parameters,
/// `seed`, `response_format` and `tools` are mapped, and `continue_final_message` (vLLM) continues
/// the last assistant message; other parameters are ignored.
pub fn chat(body: Value) -> Result<(Request, bool), BadRequest> {
    let o = object(body)?;
    let messages = match o.get("messages") {
        Some(Value::Array(m)) if !m.is_empty() => m.iter().map(message).collect::<Result<Vec<_>, _>>()?,
        _ => return Err(bad("`messages` must be a non-empty array")),
    };
    let stream = o.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let continues = flag(&o, "continue_final_message")?;
    if continues {
        if flag(&o, "add_generation_prompt")? {
            return Err(bad("`continue_final_message` and `add_generation_prompt` cannot both be true"));
        }
        if !messages.last().is_some_and(|m| m.role == Role::Assistant && m.tool_calls.is_empty()) {
            return Err(bad("`continue_final_message` needs the last message to be an assistant message without tool calls"));
        }
    }
    let mut req = Request::messages(messages);
    if continues {
        req = req.continuing();
    }
    sampling_and_seed(&o, &mut req)?;
    req.response_format = response_format(o.get("response_format"))?;
    req.tools = tools(o.get("tools"))?;
    Ok((req, stream))
}

/// `model`, sampling parameters and `seed`, shared by chat and text completions.
fn sampling_and_seed(o: &Map<String, Value>, req: &mut Request) -> Result<(), BadRequest> {
    req.model = model(o)?;
    let s = &mut req.sampling;
    s.temperature = f32_of(o, "temperature")?;
    s.top_p = f32_of(o, "top_p")?;
    s.top_k = u32_of(o, "top_k")?;
    s.min_p = f32_of(o, "min_p")?;
    s.max_tokens = match u32_of(o, "max_completion_tokens")? {
        Some(n) => Some(n),
        None => u32_of(o, "max_tokens")?,
    };
    s.stop = match o.get("stop") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(_) => return Err(bad("`stop` must be a string or an array of strings")),
    };
    s.presence_penalty = f32_of(o, "presence_penalty")?;
    s.frequency_penalty = f32_of(o, "frequency_penalty")?;
    s.repeat_penalty = match f32_of(o, "repeat_penalty")? {
        Some(p) => Some(p),
        None => f32_of(o, "repetition_penalty")?,
    };
    req.seed = match o.get("seed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad("`seed` must be a whole number"))?),
    };
    req.cache = cache(o.get("cache"))?;
    Ok(())
}

/// The `cache` body parameter (a weirdnet extension): absent, `null`, `true` or `"use"` uses the
/// node's answer cache; `false`, `"bypass"` or `"off"` skips it; `"refresh"` skips the read and
/// stores the fresh answer.
fn cache(v: Option<&Value>) -> Result<CacheMode, BadRequest> {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(true)) => Ok(CacheMode::Use),
        Some(Value::Bool(false)) => Ok(CacheMode::Bypass),
        Some(Value::String(s)) => cache_word(s).ok_or_else(|| bad("`cache` must be true, false, \"use\", \"bypass\" or \"refresh\"")),
        Some(_) => Err(bad("`cache` must be true, false, \"use\", \"bypass\" or \"refresh\"")),
    }
}

/// A cache mode by name: `use`/`on`, `bypass`/`off`, `refresh` (any case).
fn cache_word(s: &str) -> Option<CacheMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "use" | "on" => Some(CacheMode::Use),
        "bypass" | "off" => Some(CacheMode::Bypass),
        "refresh" => Some(CacheMode::Refresh),
        _ => None,
    }
}

/// The `x-weirdnet-cache` request header: `off` (or `bypass`), `refresh`, or `use` (or `on`).
pub fn cache_header(value: &str) -> Result<CacheMode, BadRequest> {
    cache_word(value).ok_or_else(|| bad("`x-weirdnet-cache` must be off, refresh or use"))
}

/// The stricter of two cache modes: `bypass` over `refresh` over `use`.
pub fn stricter(a: CacheMode, b: CacheMode) -> CacheMode {
    fn rank(m: CacheMode) -> u8 {
        match m {
            CacheMode::Use => 0,
            CacheMode::Refresh => 1,
            // Bypass, and any later mode: neither read nor store.
            _ => 2,
        }
    }
    if rank(b) > rank(a) { b } else { a }
}

/// A `/v1/completions` body: a raw-prompt request and whether to stream. `prompt` is a string
/// (or an array of one string); sampling parameters and `seed` map as for chat. `n`, `best_of`
/// above 1, `echo`, `suffix` and `logprobs` are refused; other parameters are ignored.
pub fn completion(body: Value) -> Result<(Request, bool), BadRequest> {
    let o = object(body)?;
    let prompt = match o.get("prompt") {
        Some(Value::String(p)) => p.clone(),
        Some(Value::Array(a)) if a.len() == 1 && a[0].is_string() => a[0].as_str().unwrap_or_default().to_string(),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => return Err(bad("one prompt per request")),
        _ => return Err(bad("`prompt` must be a string")),
    };
    for key in ["n", "best_of"] {
        if u32_of(&o, key)?.is_some_and(|n| n > 1) {
            return Err(bad(format!("`{key}` above 1 is not supported")));
        }
    }
    if o.get("echo").and_then(Value::as_bool).unwrap_or(false) {
        return Err(bad("`echo` is not supported"));
    }
    for key in ["suffix", "logprobs"] {
        if o.get(key).is_some_and(|v| !v.is_null() && *v != Value::Bool(false)) {
            return Err(bad(format!("`{key}` is not supported")));
        }
    }
    let stream = o.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut req = Request::prompt(prompt);
    sampling_and_seed(&o, &mut req)?;
    Ok((req, stream))
}

/// A `/v1/embeddings` body. Only `encoding_format: "float"`.
pub fn embed(body: Value) -> Result<EmbedRequest, BadRequest> {
    let o = object(body)?;
    let inputs = match o.get("input") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) if !a.is_empty() && a.iter().all(Value::is_string) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => return Err(bad("`input` must be a string or a non-empty array of strings")),
    };
    if o.get("encoding_format").and_then(Value::as_str).is_some_and(|f| f != "float") {
        return Err(bad("only encoding_format = \"float\" is supported"));
    }
    let mut req = EmbedRequest::new(inputs);
    req.model = model(&o)?;
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use weirdnet_agent::Role;
    use weirdnet_agent::remote::infer::v2::Input;

    #[test]
    fn maps_messages_sampling_tools_and_format() {
        let (req, stream) = chat(json!({
            "model": "m", "stream": true, "temperature": 0, "max_tokens": 9, "stop": "\n", "seed": 7, "repetition_penalty": 1.1,
            "messages": [
                {"role": "developer", "content": "be brief"},
                {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"x\":1}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "r", "schema": {"type": "object"}, "strict": true}},
            "logit_bias": {}
        }))
        .unwrap();
        assert!(stream);
        assert_eq!(req.model.as_deref(), Some("m"));
        let Input::Messages(m) = &req.input else { panic!() };
        assert_eq!(m.iter().map(|m| m.role).collect::<Vec<_>>(), [Role::System, Role::User, Role::Assistant, Role::Tool]);
        assert_eq!(m[1].content, "ab", "text-only parts become a plain string");
        assert_eq!(m[2].tool_calls[0].arguments, json!({"x": 1}));
        assert_eq!(m[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!((req.sampling.temperature, req.sampling.max_tokens, req.sampling.repeat_penalty), (Some(0.0), Some(9), Some(1.1)));
        assert_eq!((req.sampling.stop.clone(), req.seed), (vec!["\n".to_string()], Some(7)));
        assert_eq!(req.tools[0].input_schema, json!({"type": "object"}));
        assert!(matches!(req.response_format, Some(ResponseFormat::JsonSchema { strict: true, .. })));
    }

    #[test]
    fn images_and_audio_pass_through_as_parts() {
        let png = "data:image/png;base64,iVBORw0KGgo=";
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": png, "detail": "low"}},
            {"type": "image_url", "image_url": png},
            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}}
        ]}]});
        let (r, _) = chat(body).unwrap();
        let m = &r.inline_messages().unwrap()[0];
        let Content::Parts(parts) = &m.content else { panic!("parts expected") };
        assert_eq!(parts.len(), 4);
        assert!(matches!(&parts[1], ContentPart::ImageUrl { image_url } if image_url.url == png && image_url.detail.as_deref() == Some("low")));
        assert_eq!(parts[2], ContentPart::image(png));
        assert_eq!(parts[3], ContentPart::audio("UklGRg==", "wav"));
        assert!(r.needs().vision && r.needs().audio);
    }

    #[test]
    fn refuses_what_it_cannot_carry() {
        assert!(chat(json!({"messages": []})).is_err());
        assert!(chat(json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {}}]}]})).is_err());
        let remote = json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]}]});
        assert!(chat(remote).unwrap_err().0.contains("data:image"));
        assert!(chat(json!({"messages": [{"role": "user", "content": [{"type": "file", "file": {}}]}]})).unwrap_err().0.contains("`file`"));
        assert!(chat(json!({"messages": [{"role": "user", "content": "x"}], "temperature": "hot"})).is_err());
        assert!(chat(json!([1])).is_err());
        assert!(embed(json!({"input": "x", "encoding_format": "base64"})).is_err());
        assert!(completion(json!({"prompt": ["a", "b"]})).is_err());
        assert!(completion(json!({"prompt": "a", "n": 2})).is_err());
        assert!(completion(json!({"prompt": "a", "echo": true})).is_err());
        assert!(completion(json!({"prompt": "a", "logprobs": 2})).is_err());
        assert!(completion(json!({"prompt": 3})).is_err());
        assert_eq!(embed(json!({"input": ["a", "b"], "model": "e"})).unwrap().inputs, ["a", "b"]);
    }

    #[test]
    fn continue_final_message_moves_the_last_assistant_message_into_the_prefix() {
        let msgs = json!([{"role": "user", "content": "q"}, {"role": "assistant", "content": "Hel"}]);
        let (req, _) = chat(json!({"messages": msgs, "continue_final_message": true, "add_generation_prompt": false})).unwrap();
        assert_eq!(req.inline_messages().unwrap().len(), 1);
        assert_eq!(req.assistant_prefix.as_ref().map(|p| p.text.as_str()), Some("Hel"));
        // Without the flag a trailing assistant message stays a message.
        let (req, _) = chat(json!({"messages": msgs})).unwrap();
        assert_eq!((req.inline_messages().unwrap().len(), req.assistant_prefix.is_none()), (2, true));
        assert!(chat(json!({"messages": msgs, "continue_final_message": true, "add_generation_prompt": true})).is_err());
        assert!(chat(json!({"messages": [{"role": "user", "content": "q"}], "continue_final_message": true})).is_err());
        assert!(chat(json!({"messages": msgs, "continue_final_message": "yes"})).is_err());
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_a_prompt_and_its_sampling() {
        let (req, stream) = completion(
            json!({"model": "base", "prompt": ["Once"], "max_tokens": 5, "temperature": 0.5, "stop": ["\n"], "seed": 3, "n": 1, "logprobs": null}),
        )
        .unwrap();
        assert!(!stream);
        assert_eq!((req.raw_prompt(), req.model.as_deref()), (Some("Once"), Some("base")));
        assert_eq!((req.sampling.max_tokens, req.sampling.temperature, req.seed), (Some(5), Some(0.5), Some(3)));
        assert_eq!(req.sampling.stop, ["\n"]);
        assert!(req.tools.is_empty());
    }

    #[test]
    fn cache_parameter_and_header() {
        let base = || json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let with = |v: Value| {
            let mut b = base();
            b["cache"] = v;
            chat(b).map(|(r, _)| r.cache)
        };
        assert_eq!(chat(base()).unwrap().0.cache, CacheMode::Use);
        assert_eq!(with(json!(null)), Ok(CacheMode::Use));
        assert_eq!(with(json!(true)), Ok(CacheMode::Use));
        assert_eq!(with(json!(false)), Ok(CacheMode::Bypass));
        assert_eq!(with(json!("refresh")), Ok(CacheMode::Refresh));
        assert_eq!(with(json!("Bypass")), Ok(CacheMode::Bypass));
        assert!(with(json!("sometimes")).is_err());
        assert!(with(json!(0)).is_err());
        let (r, _) = completion(json!({"prompt": "Once", "cache": false})).unwrap();
        assert_eq!(r.cache, CacheMode::Bypass);

        assert_eq!(cache_header("off"), Ok(CacheMode::Bypass));
        assert_eq!(cache_header(" REFRESH "), Ok(CacheMode::Refresh));
        assert_eq!(cache_header("use"), Ok(CacheMode::Use));
        assert!(cache_header("hit").is_err());
        assert_eq!(stricter(CacheMode::Use, CacheMode::Refresh), CacheMode::Refresh);
        assert_eq!(stricter(CacheMode::Bypass, CacheMode::Refresh), CacheMode::Bypass);
        assert_eq!(stricter(CacheMode::Refresh, CacheMode::Use), CacheMode::Refresh);
    }
}
