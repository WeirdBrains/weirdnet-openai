# Changelog

## [Unreleased]

## [0.1.0] - 2026-11-19

### Added

- `Bridge`: a loopback-only OpenAI-compatible server over a `weirdnet-agent` 0.4 `Pool`: `/v1/chat/completions` (streaming and not, reasoning, tool calls, usage), `/v1/models` from the pool catalog, `/v1/embeddings` and Prometheus `/metrics`.
- Bearer token (at least 16 characters, constant-time check, `new_token`), `Host` check against DNS rebinding, no CORS.
- `x-weirdnet-model`, `x-weirdnet-cache` and `x-weirdnet-standin` headers; `x-wait: 1` waits for the requested model.
- `request`: OpenAI bodies to infer/2 requests.
