# weirdnet-openai agents

The collective `AGENTS.md` in `WeirdBrains/weirdnet-agents` is mandatory and wins on conflict.

- Loopback only, token required, Host checked, no CORS: never relax these.
- No app-specific types or names; the bridge only speaks to a `weirdnet-agent` `Pool`.
- Build outside the source checkout, with `CARGO_TARGET_DIR` on a scratch disk; clean targets afterwards.
