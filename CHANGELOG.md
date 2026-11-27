# Changelog

## [Unreleased]

### Added

- `video_url` content parts in chat messages (a `data:video/...` URL, as an object or a bare string) go to the pool as `ContentPart::VideoUrl`; remote and non-video URLs are a 400 (nothing is fetched).
- `POST /v1/audio/speech` over `Pool::speak` and `Pool::speak_stream` (infer/2 `Speak`): OpenAI's `input`, `model`, `voice`, `instructions`, `response_format`, `speed` and `stream_format: "audio"`, plus `weirdnet.{language, seed, sample_rate, design, ref_audio, ref_text, consent, script, raw}`. Checked against the catalog first: unknown voices such as `alloy` are a 400 listing the model's presets, never remapped. `Content-Type` carries the real format and rate, with `x-audio-sample-rate`, `x-weirdnet-audio-channels` and `x-weirdnet-ignored`. `request::speech` parses the body.
- `POST /v1/audio/transcriptions` (multipart) over `Pool::transcribe`, answering `json`, `verbose_json`, `text`, `srt` or `vtt`.
- `/v1/models` has `weirdnet.video`, `weirdnet.inputs` (`{vision, audio, video}`) and `weirdnet.speech` (a speaking model's profile, when reported); `weirdnet.limits` now includes the video limits.

### Changed

- Needs weirdnet-agent 0.5.11. CI runs on Linux, macOS and Windows.

## [0.2.4] - 2026-11-26

### Added

- `Bridge::with_max_request_bytes` and `DEFAULT_MAX_REQUEST_BYTES` (32 MiB): a configurable request size limit; larger bodies get a 413 that names it.
- `/v1/models` has `weirdnet.max_request_bytes` and `weirdnet.limits` (catalog/1 `CatalogModel::limits`, weirdnet-agent 0.5.7).

## [0.2.3] - 2026-11-25

### Added

- `/v1/models` has `weirdnet.origin` (`hub`, `upload` or `train`) and `weirdnet.license` when the node says (catalog/1 `CatalogModel::origin` and `license`), `null` otherwise.

### Changed

- Needs weirdnet-agent 0.5.4.

## [0.2.2] - 2026-11-23

### Added

- `"adapter_scale"` in a chat or completion body sets how strongly a LoRA adapter model applies for that request (infer/2 `Request::adapter_scale`); not a finite number is a 400.
- `/v1/models` has `weirdnet.base`: a LoRA adapter's base model (catalog/1 `CatalogModel::base`), `null` for models.

### Changed

- Needs weirdnet-agent 0.5.2.

## [0.2.1] - 2026-11-22

### Added

- `"cache": false` or `x-weirdnet-cache: off` skips the nodes' answer cache; `"cache": "refresh"` or `x-weirdnet-cache: refresh` refreshes it (infer/2 `Request::cache`). The stricter of body and header wins; other values are a 400.
- `request::cache_header` and `request::stricter`.

### Changed

- Requires weirdnet-agent 0.5.1.

## [0.2.0] - 2026-11-20

### Added

- Chat messages carry `image_url` (data URLs) and `input_audio` parts to the pool instead of answering 400; the pool routes them to a model that takes them, or the answer is a 400 that says none does.
- `/v1/models` lists `weirdnet.vision` and `weirdnet.audio`.

### Changed

- Needs `weirdnet-agent` 0.5 (content parts). Request bodies may be up to 32 MiB (was 16 MiB).

## [0.1.2] - 2026-11-19

### Added

- Chat `continue_final_message: true` (vLLM's parameter) continues the last assistant message through the pool (`weirdnet-agent` 0.4.2 `Request::continuing`). The reply, whole or streamed, holds only the new text; with no node that can continue it is a 501. With `add_generation_prompt: true`, or a last message that isn't a plain assistant message, it is a 400.

### Changed

- Depends on `weirdnet-agent` `"0.4.2"`.

## [0.1.1] - 2026-11-19

### Added

- `/v1/completions` works: the prompt goes to the pool as an infer/2 raw prompt (`weirdnet-agent` 0.4.1), whole or streamed (`text_completion` chunks). Sampling parameters and `seed` map as for chat; `n`/`best_of` above 1, `echo`, `suffix` and `logprobs` are refused with 400; a node that can't take raw prompts answers 501.
- Streamed answers carry `x-weirdnet-cache: hit` too: after a node accepts, headers wait up to `EARLY_META_WAIT` (1.5 s) for its early metadata or first output. Nodes before 0.4.1 report hits only at the end, in the final chunk's `weirdnet.cache_hit` and the metric.
- `/v1/embeddings` reports the node's real `usage.prompt_tokens` (0 from nodes before 0.4.1).

### Fixed

- A node's `BadRequest` on `/v1/embeddings` (say, a model that does not embed) is a 400 again, not a 503: the pool keeps error kinds (`weirdnet-agent` 0.4.1).

### Changed

- Depends on `weirdnet-agent` `"0.4.1"` (a caret requirement, not an exact pin).
- A cache hit found only at the end of a stream is counted once in `weirdnet_bridge_cache_hits_total`.

## [0.1.0] - 2026-11-19

### Added

- `Bridge`: a loopback-only OpenAI-compatible server over a `weirdnet-agent` 0.4 `Pool`: `/v1/chat/completions` (streaming and not, reasoning, tool calls, usage), `/v1/models` from the pool catalog, `/v1/embeddings` and Prometheus `/metrics`.
- Bearer token (at least 16 characters, constant-time check, `new_token`), `Host` check against DNS rebinding, no CORS.
- `x-weirdnet-model`, `x-weirdnet-cache` and `x-weirdnet-standin` headers; `x-wait: 1` waits for the requested model.
- `request`: OpenAI bodies to infer/2 requests.
