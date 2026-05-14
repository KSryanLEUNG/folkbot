# Folkbot

> Always-on family AI agent in Rust. One shared identity for the whole household, structured persistent memory, multimodal Telegram + CLI.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Made with Rust](https://img.shields.io/badge/built_with-Rust-orange.svg)](https://www.rust-lang.org)

## What it is

Most AI chatbots give every user a fresh, isolated session. Folkbot is the opposite: **one persistent personality with one shared memory** that every family member talks to. The agent learns who's who through conversation, remembers things across sessions and channels, and respects who's allowed to teach it what.

Designed to live on a NUC / Pi / mini-PC and stay coherent over months without dragging context cost up.

## Highlights

- **One soul, many people** — single shared identity (the "soul card") that owners can let the agent edit through audited patches
- **Three-tier memory** — durable facts (importance-tagged) + rolling daily/weekly/monthly summaries + sliding-window history; designed for low-cost long-running deployment
- **Multimodal** — photos, voice (Whisper), stickers, documents in; files (`send_file`) and proactive messages (`send_message`) out
- **Group-aware** — Telegram group chats become shared rooms with their own timeline; addressing detection (mention / reply / soul-trigger words like nicknames) decides when Folkbot speaks vs. just listens
- **Role-based access** — `owner` / `vice_owner` / `regular` gates who can change Folkbot's identity, read raw cross-user transcripts, send proactive messages, etc.
- **Provider-agnostic** — any OpenAI-compatible endpoint (Poe, OpenAI, Together, Groq, OpenRouter, Ollama, …); MCP servers plug in for extra tools (filesystem, fetch, your own)
- **Single binary** — `cargo install --path .` and you're done; SQLite persistence, no external services required

## Quick start

```bash
# 1. clone + build
git clone https://github.com/ryanrain-ifsh/folkbot.git
cd folkbot
cargo install --path .

# 2. config
cp folkbot.toml.example folkbot.toml
# edit: set your LLM provider, API key env var, optional Telegram bot token + allowlist

# 3. run
folkbot              # interactive REPL (default)
folkbot serve        # daemon mode (Telegram + any other channels)
folkbot --help       # all subcommands
```

First-run flow: type your name → Folkbot calls `user_identify` → links you to this CLI principal → next time it knows who you are. Use `folkbot user set-role <you> owner` to grant yourself permission to edit Folkbot's soul card.

Minimal `folkbot.toml`:

```toml
[llm]
provider = "openai"
base_url = "https://api.openai.com/v1"   # or any OpenAI-compatible
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"

[agent]
system_prompt = "You are a family AI assistant. Keep replies short and natural."

# Optional: Telegram channel
# [channels.telegram]
# bot_token_env = "TELEGRAM_BOT_TOKEN"
# allowed_users = ["123456789"]
```

See [`folkbot.toml.example`](folkbot.toml.example) for the full annotated config.

## Documentation

- **[docs/architecture/](docs/architecture/)** — 8 mermaid diagrams covering process topology, turn lifecycle, data schema, prompt composition, trust boundaries, tool system, and multimodal flow
- **[docs/agent-playbook.md](docs/agent-playbook.md)** — 7-phase development playbook generalised from how Folkbot was built (useful for other AI-agent projects)

## Status

This is a personal project I run for my own household. Feature-complete for what I need today; future work focuses on additional channels (Discord), audio-provider routing, and richer MCP integrations.

Issues and PRs welcome, but treat the codebase as opinionated — many decisions (single shared identity, no per-user sessions, owner-only soul edits) are intentional design constraints, not oversights.

## License

MIT — see [LICENSE](LICENSE).
