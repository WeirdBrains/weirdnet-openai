# Handoff

Running log for [weirdnet-openai](https://github.com/WeirdBrains/weirdnet-openai), newest first. Shared rules: the collective `AGENTS.md` and this repo's [AGENTS.md](AGENTS.md).

## 2026-11-22: skip or refresh the answer cache per request (branch `cache-mode`, for 0.2.1)

What changed ([`949e173`](https://github.com/WeirdBrains/weirdnet-openai/commit/949e17373a9f500953087882c3bdf75b64a0328f)): chat and text completions map `"cache": false` (or `"bypass"`/`"off"`) and the `x-weirdnet-cache: off` header to [weirdnet-agent](https://github.com/WeirdBrains/weirdnet-agent) 0.5.1's `CacheMode::Bypass`, and `"cache": "refresh"` / `x-weirdnet-cache: refresh` to `Refresh`. When both are given the stricter wins (bypass over refresh over use); any other value is a 400, so an eval never silently reads the cache. The `x-weirdnet-cache: hit` reply header is unchanged. New public helpers `request::cache_header` and `request::stricter`. Requested for [Ymir](https://github.com/WeirdBrains/ymir)'s training evals. Additive, so a patch release, 0.2.1.

Branch only ([`9c1f130`](https://github.com/WeirdBrains/weirdnet-openai/commit/9c1f130eb53678156ea33fcf65f44c5b0b75410d) and `Cargo.toml`): a `[patch.crates-io]` git dependency on weirdnet-agent `64fadb3`, CI on this branch, and `cargo package` skipped, because weirdnet-agent 0.5.1 isn't on crates.io yet.

Checks run: `cargo fmt --check`, `cargo clippy --all-targets` with and without `--all-features` (`-D warnings`), `cargo test --all-features` (unit tests for the parameter and header, and a bridge test that the mode reaches the node), `cargo test --no-default-features`, `cargo doc` with `-D warnings`, all against the patched weirdnet-agent.

Not tested: `cargo package` (it ignores `[patch]`); embeddings have no cache mode on the wire and ignore the parameter.

What's left and next: after weirdnet-agent 0.5.1 is published, revert the CI commit, require `weirdnet-agent = "0.5.1"`, drop the `[patch.crates-io]` section, run the checks and `cargo package`, merge to main and release 0.2.1 once Michael OKs it.
