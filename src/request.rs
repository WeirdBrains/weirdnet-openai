//! OpenAI request bodies to `weirdnet-agent` infer/2 requests.

use serde_json::{Map, Value};
use weirdnet_agent::remote::infer::v2::{EmbedRequest, Request, ResponseFormat};
use weirdnet_agent::{Message, ToolCall, ToolSpec};

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

/// Message content: a string, or text parts joined. Other parts (images, audio) are refused.
fn content(v: Option<&Value>) -> Result<String, BadRequest> {
    match v {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                match p["type"].as_str() {
                    Some("text") => out.push_str(p["text"].as_str().unwrap_or_default()),
                    Some(other) => return Err(bad(format!("content parts of type `{other}` are not supported"))),
                    None => return Err(bad("content parts need a `type`")),
                }
            }
            Ok(out)
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

/// A `/v1/chat/completions` body: the request and whether to stream. Known sampling parameters,
/// `seed`, `response_format` and `tools` are mapped; other parameters are ignored.
pub fn chat(body: Value) -> Result<(Request, bool), BadRequest> {
    let o = object(body)?;
    let messages = match o.get("messages") {
        Some(Value::Array(m)) if !m.is_empty() => m.iter().map(message).collect::<Result<Vec<_>, _>>()?,
        _ => return Err(bad("`messages` must be a non-empty array")),
    };
    let stream = o.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut req = Request::messages(messages);
    req.model = model(&o)?;
    let s = &mut req.sampling;
    s.temperature = f32_of(&o, "temperature")?;
    s.top_p = f32_of(&o, "top_p")?;
    s.top_k = u32_of(&o, "top_k")?;
    s.min_p = f32_of(&o, "min_p")?;
    s.max_tokens = match u32_of(&o, "max_completion_tokens")? {
        Some(n) => Some(n),
        None => u32_of(&o, "max_tokens")?,
    };
    s.stop = match o.get("stop") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(_) => return Err(bad("`stop` must be a string or an array of strings")),
    };
    s.presence_penalty = f32_of(&o, "presence_penalty")?;
    s.frequency_penalty = f32_of(&o, "frequency_penalty")?;
    s.repeat_penalty = match f32_of(&o, "repeat_penalty")? {
        Some(p) => Some(p),
        None => f32_of(&o, "repetition_penalty")?,
    };
    req.seed = match o.get("seed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad("`seed` must be a whole number"))?),
    };
    req.response_format = response_format(o.get("response_format"))?;
    req.tools = tools(o.get("tools"))?;
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
        assert_eq!(m[1].content, "ab");
        assert_eq!(m[2].tool_calls[0].arguments, json!({"x": 1}));
        assert_eq!(m[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!((req.sampling.temperature, req.sampling.max_tokens, req.sampling.repeat_penalty), (Some(0.0), Some(9), Some(1.1)));
        assert_eq!((req.sampling.stop.clone(), req.seed), (vec!["\n".to_string()], Some(7)));
        assert_eq!(req.tools[0].input_schema, json!({"type": "object"}));
        assert!(matches!(req.response_format, Some(ResponseFormat::JsonSchema { strict: true, .. })));
    }

    #[test]
    fn refuses_what_it_cannot_carry() {
        assert!(chat(json!({"messages": []})).is_err());
        assert!(chat(json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {}}]}]})).is_err());
        assert!(chat(json!({"messages": [{"role": "user", "content": "x"}], "temperature": "hot"})).is_err());
        assert!(chat(json!([1])).is_err());
        assert!(embed(json!({"input": "x", "encoding_format": "base64"})).is_err());
        assert_eq!(embed(json!({"input": ["a", "b"], "model": "e"})).unwrap().inputs, ["a", "b"]);
    }
}
