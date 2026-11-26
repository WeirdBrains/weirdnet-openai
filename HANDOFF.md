# Handoff

Running log for [weirdnet-openai](https://github.com/WeirdBrains/weirdnet-openai), newest first. Shared rules: the collective `AGENTS.md` and this repo's [AGENTS.md](AGENTS.md).


## 2026-11-26: configurable request size limit (for 0.2.4)

What changed: [`fdeb834`](https://github.com/WeirdBrains/weirdnet-openai/commit/fdeb83475b0b1997b2847c0c741a63ec8f9f8ba2) replaces the fixed 32 MiB body limit with `Bridge::with_max_request_bytes(usize)` (read back with `Bridge::max_request_bytes()`; default `DEFAULT_MAX_REQUEST_BYTES` = 32 MiB). `BridgeOptions` keeps its fields: adding one would break struct literals, so the setting lives on `Bridge`. A body over the limit gets 413 `request_too_large` whose message names the limit in bytes and in KiB/MiB, plus an `x-weirdnet-max-request-bytes` header; the rest of the body is drained (up to 64 MiB more) so clients read the 413 instead of a broken pipe, for `Content-Length` and chunked bodies alike. `/v1/models` adds `weirdnet.max_request_bytes` (the smaller of the bridge's and the node's) and `weirdnet.limits` ([weirdnet-agent](https://crates.io/crates/weirdnet-agent) 0.5.7 `CatalogModel::limits`). `weirdnet-agent` floor raised to 0.5.7.

Checks run: `cargo fmt --check`; `cargo clippy --all-targets -D warnings` (default and all features); `cargo test` default/all features and `--no-default-features` (6 unit, 11 bridge tests, including the default 32 MiB limit, a custom 64 KiB limit admitting a body of exactly that size and refusing one byte more, chunked bodies, and `/v1/models`); `cargo doc -D warnings`; `cargo semver-checks` against 0.2.3: no semver update required.

Not tested: real clients other than reqwest; limits above 32 MiB end to end (infer/2 carries at most 32 MiB per message).

What's left and next: release 0.2.4; `video_url` handling later.

## 2026-11-25: released 0.2.3 (/v1/models origin and license)

What changed: the /v1/models origin and license work below, released with `scripts/weirdnet_release.py --patch` from main at [`bf765d0`](https://github.com/WeirdBrains/weirdnet-openai/commit/bf765d0006b70e10e5d02666863b197d3677796e) (CI green). Release commit [`5bca803`](https://github.com/WeirdBrains/weirdnet-openai/commit/5bca803b00c0dc6406e067bdf5cf88c8f8a4b9ee), tag [`v0.2.3`](https://github.com/WeirdBrains/weirdnet-openai/tree/v0.2.3), on [crates.io](https://crates.io/crates/weirdnet-openai/0.2.3) and [docs.rs](https://docs.rs/weirdnet-openai/0.2.3). No GitHub release page; the tag and the changelog carry the notes.

Release basis: m3talsmith's standing rule of 2026-11-24 for additive Weirdnet patch releases; `cargo semver-checks` found nothing that needs more than a patch.

Checks run: the release script's checks (fmt, clippy `-D warnings --all-features`, tests `--all-features`, `cargo doc -D warnings`) and `cargo publish` verification.

Not tested: no live node or pool.

What's left and next: nothing for this release; the site pages follow.

## 2026-11-25: `/v1/models` origin and license (for 0.2.3)

What changed ([`5ae3036`](https://github.com/WeirdBrains/weirdnet-openai/commit/5ae30369eb381bcf887c61047e1025511a88057f), branch `feat/catalog-origin`, merged to main with `--no-ff`): `/v1/models` lists `weirdnet.origin` (`hub`, `upload` or `train`) and `weirdnet.license` per model from catalog/1 `CatalogModel::origin`/`license` ([weirdnet-agent](https://crates.io/crates/weirdnet-agent) 0.5.4, now the minimum), `null` when the node does not say. Additive JSON fields: a patch release, 0.2.3.

Checks run: `cargo fmt --check`, `cargo build`, `cargo clippy --all-targets` with and without `--all-features` (`-D warnings`), `cargo test --all-features` (6 unit, 9 bridge; the multimodal bridge test now checks both fields on an adapter that has them and `null` on a model without), `cargo doc --no-deps --all-features` with `-D warnings`, and `cargo semver-checks` against 0.2.2 (no semver update required).

Not tested: no live pool; only the in-memory catalog in the bridge tests.

Release basis: m3talsmith's standing rule of 2026-11-24 allows additive Weirdnet patch releases without asking; minor bumps still need his OK.

What's left and next: release 0.2.3 once CI on main is green.

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
