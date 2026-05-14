# Folkbot — Architecture Reference

Detailed architecture diagrams (mermaid format — GitHub / VSCode render directly; locally install `markdown-preview-mermaid` if needed).

Each diagram focuses on one perspective; don't try to stuff everything into one diagram.

| # | File | Perspective | Good for answering |
|---|---|---|---|
| 1 | [01-process-topology.md](01-process-topology.md) | **runtime topology** | When `folkbot serve` is running, what tasks / children live inside the process? |
| 2 | [02-turn-lifecycle.md](02-turn-lifecycle.md) | **single-turn flow** | A message comes in → a reply goes out, what runs in between? |
| 3 | [03-data-schema.md](03-data-schema.md) | **persistent state** | What tables, relations, and indexes are in the DB? |
| 4 | [04-module-deps.md](04-module-deps.md) | **module dependency** | How do modules inside the crate import each other? |
| 5 | [05-context-pyramid.md](05-context-pyramid.md) | **prompt composition** | How is each turn's system prompt assembled? |
| 6 | [06-trust-boundaries.md](06-trust-boundaries.md) | **security / privacy** | How is trust designed across role / channel / tools? |
| 7 | [07-tool-system.md](07-tool-system.md) | **tool dispatch** | What path does the LLM take when calling tools? built-in vs MCP? |
| 8 | [08-multimodal.md](08-multimodal.md) | **media ingest (v1.4)** | How do images / voice / stickers / documents get ingested? |

---

## Conventions

- **Bold inside boxes** = persistent storage or external system
- **Arrows** = data flow; double arrows = bidirectional
- **subgraph frames** = trust boundary / process boundary / abstraction-layer boundary
- **Colors** (mermaid `classDef`):
  - Yellow = entry point / main task
  - Blue = channel adapter
  - Pink = tool / MCP
  - Green = storage
  - Purple = LLM
  - Gray = infra / utility

---

## How this differs from README.md

The top-level `README.md` is user-facing: "what is this, how do I configure it, what commands are there".
This folder is developer-facing: "the internal structure of the program, what logic lives where, why it's split this way".

If you change the architecture (add a channel, add a trust level, change the schema), please update the corresponding file too.
