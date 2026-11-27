# weirdnet-openai

A loopback-only OpenAI-compatible bridge over a
[`weirdnet-agent`](https://crates.io/crates/weirdnet-agent) 0.5 inference `Pool`. Local tools
that speak the OpenAI API (editors, notebooks, `curl`) reach the user's inference nodes
through it.

| Route | |
|---|---|
| `POST /v1/chat/completions` | streaming (SSE, `data: [DONE]`) or not; reasoning as `reasoning_content`; tool calls; usage |
| `GET /v1/models` | the pool catalog, with `weirdnet: {state, purpose, replicas, context, revision, vision, audio, video, inputs, base, origin, license, max_request_bytes, limits, speech}` (`purpose` is `chat`, `embed`, `transcribe`, `speak`, ...; `inputs` is `{vision, audio, video}`; `origin` is `hub`, `upload` or `train` and `license` an SPDX id when the node says, else `null`; `limits` holds the media limits the node knows, video ones included, and `max_request_bytes`, the smaller of the bridge's and the node's; `speech` is a speaking model's profile, present only when reported) |
| `POST /v1/completions` | a raw prompt (one per request), streaming or not; usage; 501 from nodes that can't take raw prompts |
| `POST /v1/embeddings` | float vectors; `usage.prompt_tokens` from the node |
| `POST /v1/audio/speech` | text to speech over infer/2 `Speak`: whole, or streamed with `stream_format: "audio"`; `Content-Type` is the real format (`audio/pcm;rate=24000` for raw PCM), with `x-audio-sample-rate` |
| `POST /v1/audio/transcriptions` | multipart; speech to text over infer/2 `Transcribe` as `json`, `verbose_json`, `text`, `srt` or `vtt` |
| `GET /metrics` | Prometheus: `weirdnet_bridge_{requests,errors,cache_hits,standin}_total` |

Security: it binds `127.0.0.1` only, checks the `Host` header against its own address (DNS
rebinding), requires a bearer token of at least 16 characters (`new_token()` makes one;
compared in constant time) and sends no CORS headers, so web pages can't call it.

Headers: `x-weirdnet-model` (the model that answered), `x-weirdnet-cache: hit` and
`x-weirdnet-standin: <model>` when the pool says so. Send `x-wait: 1` to wait for the requested
model instead of a stand-in. Non-streaming answers also carry
`weirdnet: {standin, cache_hit, served_by}`; streams carry it on the final chunk. A stream's
headers are sent once a node accepted the request and sent its early metadata or first output
(at most `EARLY_META_WAIT`, 1.5 s, later), so `x-weirdnet-cache: hit` is on streams too; from
nodes before `weirdnet-agent` 0.4.1 the cache flag arrives only in the final chunk.

Answer cache: send `"cache": false` or `x-weirdnet-cache: off` to skip the nodes' answer cache
(evals, freshly fine-tuned weights), or `"cache": "refresh"` / `x-weirdnet-cache: refresh` to
skip the read and store the fresh answer. When both are given the stricter wins; any other value
is a 400. This is the infer/2 `Request::cache` field (`weirdnet-agent` 0.5.1); nodes older than
that ignore it.

```rust
use std::sync::Arc;
use weirdnet_openai::{Bridge, BridgeOptions, new_token};

let token = new_token(); // show it to the user once; keep it in their keychain
let bridge = Bridge::bind(BridgeOptions::new(11435, token)).await?;
bridge.serve(Arc::new(pool), shutdown).await?;
```

Requests map onto infer/2: messages (string content or parts: `text`, `image_url` with a
`data:image/...` URL, `input_audio` with `data` and `format`, `video_url` with a
`data:video/...` URL; remote image or video URLs and other part types are refused with 400), `temperature`, `top_p`, `top_k`, `min_p`, `max_tokens`/`max_completion_tokens`,
`stop`, `presence_penalty`, `frequency_penalty`, `repeat_penalty`/`repetition_penalty`,
`seed`, `response_format` (text, `json_object`, `json_schema`) and function `tools`.
`continue_final_message: true` (vLLM's parameter) continues the last assistant message: the
reply holds only the new text, and a pool with no node that can continue answers 501. Other
parameters are ignored. Dropping a response cancels the pool request.

Images, audio and video go to the pool unchanged (`weirdnet-agent` 0.5 content parts; video since
0.5.10). The pool sends them only to a model whose catalog entry takes them (`vision`, `audio`,
`video` in `/v1/models`): with no `model`, it picks one; a named model that lacks the input, or a
pool with no such model, is a 400 that says so (for video, the client's fallback is to send
frames as images).

## Speech

`POST /v1/audio/speech` takes OpenAI's body: `input`, `model`, `voice` (a preset name or
`{"id": name}`), `instructions`, `response_format` (`wav`, `pcm`, `mp3`, `opus`, `flac`; absent:
the model's first format, `wav` for the usual speech models, not OpenAI's `mp3`), `speed` (a
multiple of the model's pace) and `stream_format: "audio"` (raw bytes as they come; `"sse"` is
refused). Weirdnet extensions go under `weirdnet`: `language`, `seed`, `sample_rate`, `design` (a
voice described in words), `ref_audio` (a `data:` URL only; remote and file URLs are never
fetched) with `ref_text` and `consent: true` (a cloned voice), `script`
(`[{"speaker": 0, "text": "..."}]` instead of `input`) and `raw` (the input goes to the engine
with no template).

The request is checked against the pool catalog before it is sent, so no model fakes a feature:
OpenAI voice names such as `alloy` are never mapped to another voice; a voice, format or feature
the model lacks is a 400 that names the model and what it has, such as "model `tts` has no
preset voice `alloy` (presets: vivian, ryan)". Fields the model ignored (such as a reference transcript) are
named in `x-weirdnet-ignored`. A streamed answer that fails after audio has left ends the body
early; it is never spliced with another node's take.

## Transcription

`POST /v1/audio/transcriptions` (multipart/form-data) takes `file`, `model`, `language`,
`prompt`, `response_format` (`json`, the default; `verbose_json` with `language`, `duration` and
`segments`; `text`; `srt`; `vtt`) and `timestamp_granularities[]=segment` (`word` is refused).
The audio's format comes from the file name, else its `audio/*` type, else its first bytes.
Subtitles are rendered from the transcriber's timed segments; a transcriber that times nothing
gets a 501 rather than invented cues. Neither audio endpoint is cached.

## Request size

Request bodies (chat, speech and transcription alike) may be up to 32 MiB (`DEFAULT_MAX_REQUEST_BYTES`) unless set with
`Bridge::with_max_request_bytes`. A larger body gets a 413 (`request_too_large`) whose message
names the limit, with the limit in the `x-weirdnet-max-request-bytes` header. infer/2 carries at
most 32 MiB per message, so raising the limit past that only helps bodies that shrink on the way.
