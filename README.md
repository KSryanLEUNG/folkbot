# Folkbot

> An AI that lives **with** your family — one shared personality, one persistent memory, every conversation builds on the last.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Made with Rust](https://img.shields.io/badge/built_with-Rust-orange.svg)](https://www.rust-lang.org)
[![Single binary](https://img.shields.io/badge/deploys-single%20binary-success.svg)](#quick-start)
[![Self-hosted](https://img.shields.io/badge/self--hosted-yes-brightgreen.svg)](#quick-start)

---

## The problem with most AI chatbots

Every conversation starts with amnesia. The bot forgets that your daughter has a peanut allergy, that your dad asked the same question last week, that the family group chat already decided where to eat. Every user is a stranger. Every session begins from zero.

**Folkbot is the opposite design.** One AI. One identity. One shared memory across the whole household. It learns who's who through conversation, remembers across sessions and channels, and respects who's allowed to teach it what.

Built to live on a NUC / Raspberry Pi / mini-PC and stay coherent over months — without context costs creeping up.

## What it feels like

Telling Folkbot something new (DM):

```
You:      I'm allergic to peanuts btw.
Folkbot:  noted — saved to your facts. anything specific you want me to flag?
You:      anything with hidden peanut content. asian food especially.
Folkbot:  got it.
```

Two weeks later, in the family group chat — different conversation, different speakers:

```
Dad:      let's order Thai for dinner
Sister:   yes finally
You:      sounds good
Folkbot:  heads up — most Thai dishes use peanuts. want me to call out the
          safer options on the menu when you pick a restaurant?
```

Teaching Folkbot something about itself:

```
You (owner):   btw the kids started calling you "Folky"
Folkbot:       (soul_patch applied — added "Folky" as nickname)
               ok i'll respond to that too

[next morning, the kid sends a DM]
Kid:           morning Folky
Folkbot:       hey Em, ready for school?
```

That's the whole pitch. Memory that persists. Identity that grows. Context that's shared across people and channels.

## Highlights

- **Three-tier memory** — durable facts (importance-tagged) + rolling daily / weekly / monthly summaries + sliding-window history. Context cost stays flat as the deployment ages, not linear with conversation count.

- **One soul, many people** — a single shared identity (the "soul card"). The agent can learn new traits about itself through conversation, but only the **owner** can authorize the patch. Every edit is audited.

- **Group-aware** — Telegram group chats become shared rooms with their own timeline. Mention / reply / nickname detection decides when Folkbot speaks vs. just listens.

- **Multimodal in and out** — photos, voice (Whisper-compatible), stickers, documents inbound. Files (`send_file`) and proactive messages (`send_message`) outbound.

- **Role-based access** — `owner` / `vice_owner` / `regular`. Gates who can change Folkbot's identity, read cross-user transcripts, send proactive messages.

- **Plug-in tools via MCP** — any [Model Context Protocol](https://modelcontextprotocol.io) server slots in (filesystem, web fetch, build your own).

- **Provider-agnostic** — any OpenAI-compatible endpoint (OpenAI, Together, Groq, OpenRouter, Poe, Ollama for local models, …). Swap models in one line of config.

- **Single binary, SQLite, no external services** — `cargo install --path .` and you're done. No Postgres. No Redis. No Kubernetes. (Docker images are on the roadmap — coming soon.)

## Why "one shared identity" is the right design

Most agent frameworks default to one session per user. That works for a productivity assistant. It breaks for a household, where:

- Conversations happen in group chats with multiple speakers
- The same fact ("we're vegetarian") needs to be visible to everyone
- The bot is a persona the whole family relates to, not a private oracle each person owns separately

Folkbot makes the opposite call. **Memory** is room-scoped (DM vs. group) and per-user (durable facts), but **identity** is household-scoped. See [Trust Boundaries](docs/architecture/06-trust-boundaries.md) for the full reasoning.

## Quick start

```bash
# 1. clone + build
git clone https://github.com/KSryanLEUNG/folkbot.git
cd folkbot
cargo install --path .

# 2. config
cp folkbot.toml.example folkbot.toml
# edit: pick an LLM provider, set API key env var, optional Telegram bot token

# 3. run
folkbot                # interactive REPL (default)
folkbot serve          # daemon mode (Telegram + future channels)
folkbot --help         # all subcommands
```

First-run flow: type your name → Folkbot calls `user_identify` → links you to this CLI principal → next time it knows you. Use `folkbot user set-role <you> owner` to give yourself permission to edit Folkbot's soul card.

Minimal `folkbot.toml`:

```toml
[llm]
provider = "openai"
base_url = "https://api.openai.com/v1"   # any OpenAI-compatible endpoint
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"

[agent]
system_prompt = "You are a family AI assistant. Keep replies short and natural."

# Optional Telegram channel
# [channels.telegram]
# bot_token_env = "TELEGRAM_BOT_TOKEN"
# allowed_users = ["123456789"]
```

See [`folkbot.toml.example`](folkbot.toml.example) for the fully annotated config.

## Architecture

Eight focused mermaid diagrams — each captures one perspective so no single diagram is overloaded:

| # | Perspective | Answers |
|---|---|---|
| [01](docs/architecture/01-process-topology.md) | Runtime topology | What tasks does `folkbot serve` spawn? |
| [02](docs/architecture/02-turn-lifecycle.md) | Single turn | Message in → reply out, what happens? |
| [03](docs/architecture/03-data-schema.md) | Persistent state | DB tables, relationships, indexes |
| [04](docs/architecture/04-module-deps.md) | Module deps | How do crate modules import each other? |
| [05](docs/architecture/05-context-pyramid.md) | Prompt composition | How is the system prompt assembled per turn? |
| [06](docs/architecture/06-trust-boundaries.md) | Security & privacy | Role / channel / tool trust layering |
| [07](docs/architecture/07-tool-system.md) | Tool dispatch | Built-in vs MCP dispatch path |
| [08](docs/architecture/08-multimodal.md) | Media ingest | Image / voice / sticker / doc flow |

Also see [`docs/agent-playbook.md`](docs/agent-playbook.md) — a 7-phase development playbook generalised from how Folkbot was built. Useful as a template for other agent projects.

## Roadmap

Where this is going. Issues and PRs welcome on any of these — roughly ordered by priority within each section, not across.

### Easier to run

- **One-command Docker deployment** — `docker run` with environment-only config, pre-built multi-arch images on ghcr.io (arm64 for Pi, amd64 for x86 mini-PCs)
- **`docker-compose.yml`** with sensible defaults — mounted SQLite volume, optional Ollama sidecar for fully-local deployments
- **One-click cloud templates** — Railway / fly.io / Hetzner deploy buttons
- **Hosted demo** — a public read-only sandbox so visitors can try it without setup

### More channels

- **Discord** — servers, threads, voice channels (DM and per-server soul awareness)
- **WhatsApp** — via WhatsApp Business API or `whatsapp-web.js` bridge
- **LINE** — significant adoption in Asia, important for international reach
- **Matrix / Element** — for the self-hosted-everything crowd
- **Web UI** — simple browser dashboard for users who don't want to install Telegram on every device

### Smart home integration

- **Home Assistant** — first-class integration; Folkbot becomes a Home Assistant conversation agent so family members can ask via any HA frontend
- **MQTT bridge** — subscribe to home events ("front door opened at 11pm", "garage humidity rising") and publish commands ("turn off kitchen light")
- **Matter / Thread** — direct device control where supported
- **Brand bridges** — Philips Hue, Xiaomi Mi Home / Aqara, IKEA Dirigera, Apple HomeKit (via Homebridge)
- **Routines & proactive notifications** — "tell me when the dishwasher finishes", "remind everyone bedtime is in 15 min"

### Smarter memory & reasoning

- **Vector recall** — embedding-based retrieval over old turns, complementing the importance-tagged fact store for fuzzy historical lookups
- **Fact decay & re-confirmation** — automatic importance decay; surface "is this still true?" prompts before the agent silently forgets
- **Cross-user fact sharing with consent** — "Mom can know I'm dating Alex" / "but Dad shouldn't know yet"
- **Calendar & document integration** — proactive scheduling awareness, doc-Q&A grounded in household papers (school forms, manuals, receipts)

### Operations & observability

- **Prometheus / OpenTelemetry export** — for the people running this 24/7
- **Web admin UI** — view facts, edit soul card, audit cross-user reads, manage roles from a browser
- **Backup / migration commands** — first-class export/import so households can move between machines

## Status

This is a personal project I run for my own household. It's feature-complete for what I need today; the roadmap above tracks where I'd like to take it.

Issues and PRs are welcome, but treat the codebase as opinionated — many design decisions (single shared identity, no per-user sessions, owner-only soul edits, room-scoped memory) are **intentional constraints**, not oversights. If you want to argue for changing them, please open a discussion first so I can share the reasoning that got us here.

## License

MIT — see [LICENSE](LICENSE).
