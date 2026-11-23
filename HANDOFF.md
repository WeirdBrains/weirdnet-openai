# Handoff

Running log for [weirdnet-openai](https://github.com/WeirdBrains/weirdnet-openai), newest first. Shared rules: the collective `AGENTS.md` and this repo's [AGENTS.md](AGENTS.md).


## 2026-11-23: released 0.2.2

What changed: Released with `scripts/weirdnet_release.py` (approved by Michael, 2026-11-23). Release commit [`0a3b1cc`](https://github.com/WeirdBrains/weirdnet-openai/commit/0a3b1cca06ba668ae5ae4b599077ef399f3fd40a), tag [`v0.2.2`](https://github.com/WeirdBrains/weirdnet-openai/releases/tag/v0.2.2), on [crates.io](https://crates.io/crates/weirdnet-openai/0.2.2) and [docs.rs](https://docs.rs/weirdnet-openai/0.2.2). See the entry below and [CHANGELOG.md](CHANGELOG.md).

## 2026-11-23: adapter scale and base (0.2.2)

What changed: a body's `"adapter_scale"` maps to infer/2 `Request::adapter_scale` (weirdnet-agent 0.5.2); `/v1/models` has `weirdnet.base` for LoRA adapters. See [CHANGELOG.md](CHANGELOG.md).

Checks run: fmt, clippy `-D warnings --all-features`, tests `--all-features` (6 unit, 9 bridge), `cargo doc -D warnings`.

Not tested: against a real llama.cpp node with adapters (weirdnet-backends 0.4.1 applies the scale).

## 2026-11-22: released 0.2.1

What changed: released with `scripts/weirdnet_release.py` (approved by Michael, 2026-11-22). Release commit [`0f3fb63`](https://github.com/WeirdBrains/weirdnet-openai/commit/0f3fb635b2cd6e79e385e00e8d6ec091535aee75), tag [`v0.2.1`](https://github.com/WeirdBrains/weirdnet-openai/releases/tag/v0.2.1), on [crates.io](https://crates.io/crates/weirdnet-openai/0.2.1) and [docs.rs](https://docs.rs/weirdnet-openai/0.2.1).

Checks run: the release script's checks (fmt, clippy `-D warnings`, tests, docs), `cargo publish` verification, CI on the release commit (green), the docs.rs pages for the new APIs.

Not tested: no live node.

What's left and next: nothing for this release; the [site pages](https://net.weirdbrains.com) are merged.

## 2026-11-22: ready for 0.2.1 (merge of `cache-mode`)

What changed: reverted the branch-only CI commit ([`54875cc`](https://github.com/WeirdBrains/weirdnet-openai/commit/54875ccf3b6360822c43d5e6eb87f4e85e311c82)) and moved to [weirdnet-agent 0.5.1](https://crates.io/crates/weirdnet-agent/0.5.1) from crates.io with no `[patch.crates-io]` ([`551d2c8`](https://github.com/WeirdBrains/weirdnet-openai/commit/551d2c81da8ea788044effa6bf4768798e53b28d)); merged into main with `--no-ff`. Release approved by Michael (2026-11-22).

Checks run: `cargo fmt --check`, `cargo clippy --all-targets` with and without `--all-features` (`-D warnings`), `cargo test --all-features` (6 unit and 9 bridge tests), `cargo test --no-default-features`, `cargo doc` with `-D warnings`, `cargo package --locked`.

Not tested: no live pool or inference node.

What's left and next: release 0.2.1 from the merge commit once CI on main is green, then check crates.io and docs.rs.

## 2026-11-22: skip or refresh the answer cache per request (branch `cache-mode`, for 0.2.1)

What changed ([`949e173`](https://github.com/WeirdBrains/weirdnet-openai/commit/949e17373a9f500953087882c3bdf75b64a0328f)): chat and text completions map `"cache": false` (or `"bypass"`/`"off"`) and the `x-weirdnet-cache: off` header to [weirdnet-agent](https://github.com/WeirdBrains/weirdnet-agent) 0.5.1's `CacheMode::Bypass`, and `"cache": "refresh"` / `x-weirdnet-cache: refresh` to `Refresh`. When both are given the stricter wins (bypass over refresh over use); any other value is a 400, so an eval never silently reads the cache. The `x-weirdnet-cache: hit` reply header is unchanged. New public helpers `request::cache_header` and `request::stricter`. Requested for [Ymir](https://github.com/WeirdBrains/ymir)'s training evals. Additive, so a patch release, 0.2.1.

Branch only ([`9c1f130`](https://github.com/WeirdBrains/weirdnet-openai/commit/9c1f130eb53678156ea33fcf65f44c5b0b75410d) and `Cargo.toml`): a `[patch.crates-io]` git dependency on weirdnet-agent `64fadb3`, CI on this branch, and `cargo package` skipped, because weirdnet-agent 0.5.1 isn't on crates.io yet.

Checks run: `cargo fmt --check`, `cargo clippy --all-targets` with and without `--all-features` (`-D warnings`), `cargo test --all-features` (unit tests for the parameter and header, and a bridge test that the mode reaches the node), `cargo test --no-default-features`, `cargo doc` with `-D warnings`, all against the patched weirdnet-agent.

Not tested: `cargo package` (it ignores `[patch]`); embeddings have no cache mode on the wire and ignore the parameter.

What's left and next: after weirdnet-agent 0.5.1 is published, revert the CI commit, require `weirdnet-agent = "0.5.1"`, drop the `[patch.crates-io]` section, run the checks and `cargo package`, merge to main and release 0.2.1 once Michael OKs it.
