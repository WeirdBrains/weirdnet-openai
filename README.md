# weirdnet-openai

A loopback-only OpenAI-compatible bridge over a
[`weirdnet-agent`](https://crates.io/crates/weirdnet-agent) 0.5 inference `Pool`. Local tools
that speak the OpenAI API (editors, notebooks, `curl`) reach the user's inference nodes
through it.

| Route | |
|---|---|
| `POST /v1/chat/completions` | streaming (SSE, `data: [DONE]`) or not; reasoning as `reasoning_content`; tool calls; usage |
| `GET /v1/models` | the pool catalog, with `weirdnet: {state, purpose, replicas, context, revision, vision, audio}` |
| `POST /v1/completions` | a raw prompt (one per request), streaming or not; usage; 501 from nodes that can't take raw prompts |
| `POST /v1/embeddings` | float vectors; `usage.prompt_tokens` from the node |
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

```rust
use std::sync::Arc;
use weirdnet_openai::{Bridge, BridgeOptions, new_token};

let token = new_token(); // show it to the user once; keep it in their keychain
let bridge = Bridge::bind(BridgeOptions::new(11435, token)).await?;
bridge.serve(Arc::new(pool), shutdown).await?;
```

Requests map onto infer/2: messages (string content or parts: `text`, `image_url` with a
`data:image/...` URL, `input_audio` with `data` and `format`; remote image URLs and other part
types are refused with 400), `temperature`, `top_p`, `top_k`, `min_p`, `max_tokens`/`max_completion_tokens`,
`stop`, `presence_penalty`, `frequency_penalty`, `repeat_penalty`/`repetition_penalty`,
`seed`, `response_format` (text, `json_object`, `json_schema`) and function `tools`.
`continue_final_message: true` (vLLM's parameter) continues the last assistant message: the
reply holds only the new text, and a pool with no node that can continue answers 501. Other
parameters are ignored. Dropping a response cancels the pool request.

Images and audio go to the pool unchanged (`weirdnet-agent` 0.5 content parts). The pool sends
them only to a model whose catalog entry takes them (`vision`, `audio` in `/v1/models`): with no
`model`, it picks one; a named text-only model, or a pool with no such model, is a 400 that says
so. Request bodies may be up to 32 MiB.
