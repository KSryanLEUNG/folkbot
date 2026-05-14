# Contributing to Folkbot

Thanks for thinking about contributing. This project is opinionated — please read this short doc before opening a substantial PR so we don't waste each other's time.

## Scope

Folkbot is **one shared household AI agent**. Some design choices are load-bearing and not up for refactor without discussion:

- Single shared identity (the "soul card") — no per-user sessions
- Room-scoped memory (DM vs. group) + per-user durable facts
- Owner-only edits to the soul card; audited patches
- Sliding-window history + rolling summaries + importance-tagged facts (the "three-tier memory")
- SQLite as the only data store
- Single binary, no required external services at runtime

If you want to change any of these, **open a discussion first** explaining the problem you're solving. I'd rather hear the idea early than have to reject a finished PR.

## What contributions are most welcome

Anything on the [README roadmap](README.md#roadmap) is fair game. Especially:

- **Channel adapters** — Discord, WhatsApp, LINE, Matrix, Web UI. The `OutboundChannel` trait + `channels/telegram/` are the pattern to mirror.
- **Docker packaging** — multi-arch images, `docker-compose.yml`, deploy templates.
- **Smart home integrations** — Home Assistant, MQTT bridge, Matter/Thread, individual brand bridges. Architecturally these should be MCP servers or new channels, not core changes.
- **MCP server packs** — reusable MCP servers tailored for household use (calendar, notes, photo library).
- **Bug fixes, doc fixes, test coverage** — always welcome, no discussion needed.

## What gets pushback

- Generic refactors without a concrete win (perf number, bug fixed, feature unblocked)
- New abstractions added "for future flexibility" with no current user
- Reworking the trust model (role tiers, owner gating) without first opening a discussion
- Adding dependencies that don't earn their weight — Folkbot stays a single binary
- Per-user agent personalities, multi-tenant deployments, or anything that breaks "one household, one soul"

## Development setup

```bash
git clone https://github.com/KSryanLEUNG/folkbot.git
cd folkbot
cargo build
cargo test
cargo run                # interactive REPL with default config
```

You'll need:
- Rust stable (whatever `rust-toolchain.toml` says, or just the current stable)
- An OpenAI-compatible API key set as an env var (see `.env.example`)
- For Telegram channel work: a bot token from `@BotFather`
- For MCP work: `npx` (Node) and `uvx` (Python via `uv`) on PATH

## Code style

- Match the surrounding code. Folkbot is terse, lowercase-casual in comments, technical in identifiers.
- **Defaults from the main project guidance apply**: no comments unless the *why* is non-obvious; no docstrings on every function; no premature abstractions; no scope creep beyond the PR's stated goal.
- Doc comments (`///`) on **public** APIs are encouraged when behavior isn't obvious from the signature. Skip them on internal helpers.
- Use `tracing` for logs, not `println!` (except for user-facing CLI output, where `colored::Colorize` is already the pattern).
- Errors: `anyhow::Result` at boundaries (CLI / channel handlers), structured errors only when a caller actually branches on the variant.
- SQL: prefer `sqlx::query!` for compile-time checking; avoid string concatenation.

## Tests

- Storage layer changes → add an integration test using `storage::db::test_pool()`.
- New tool → add an `invoke` test covering at least the happy path and one permission-denied case.
- Channel adapters → unit-test the addressing/parsing logic, not the network. Integration testing against live Telegram is the maintainer's problem, not yours.
- Run `cargo test` before sending the PR. If you skip a slow test, gate it behind `#[ignore]` and explain why.

## PR workflow

1. **Small, focused PRs.** One feature or fix per PR. If you find yourself touching unrelated files, that's a second PR.
2. **Title format**: `area: short imperative` — examples: `channels: add discord adapter`, `storage: index messages.room_key`, `docs: clarify soul-patch flow`.
3. **Description**: what changed, why, and how you tested. Link any related discussion.
4. **CI must pass.** If CI doesn't exist yet for the area you're touching, mention it in the PR — we'll add it together.
5. **Don't squash before review.** Rebasing on `main` is fine; force-pushes after review starts make it harder to follow.

## Discussions before code

For anything bigger than ~100 lines or touching the load-bearing pieces above, open a [Discussion](https://github.com/KSryanLEUNG/folkbot/discussions) first. A short message describing the problem and your proposed direction is enough. You'll get a fast yes / no / "here's what to be aware of" — much cheaper than building the wrong thing.

## License

By contributing, you agree your contribution is licensed under the same MIT license as the project.
