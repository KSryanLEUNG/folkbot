# 02 · Single-Turn Lifecycle — one message in to Folkbot's reply

The full flow from a user typing to Folkbot finishing its reply. The Telegram path is more complex than CLI (streaming edits, room logic), so it's the example here; CLI is a subset.

## High-level sequence

```mermaid
sequenceDiagram
    autonumber
    actor U as User
    participant TG as telegram::handle_message
    participant SC as cached_soul_triggers (5s TTL)
    participant Ed as editor task<br/>(per turn, mpsc rx)
    participant AC as AgentCore::run_turn
    participant DB as SQLite (WAL)
    participant TS as TurnState
    participant LLM as OpenAI-compat<br/>(SSE stream)
    participant T as ToolRegistry
    participant MCP as MCP child<br/>(JSON-RPC)
    participant OB as OutboundChannel

    U->>TG: text message
    TG->>TG: allowlist check (silent ignore if not)
    TG->>DB: lookup_or_create_room (if group chat)

    alt group chat & not addressed
        TG->>SC: cached soul name + nicknames
        TG->>DB: silent log into room timeline
        TG-->>U: (no reply)
    else addressed (or DM)
        TG->>U: send "…" placeholder → message_id
        TG->>Ed: spawn(rx)
        TG->>AC: run_turn(principal, text, conv_id, sink)

        AC->>DB: lookup_by_principal → current_user

        opt room turn & vibe stale > 5min
            AC->>DB: refresh_user_vibe (sync)
        end

        AC->>DB: load_personal_timeline (DM + my rooms, ≤20)
        AC->>DB: persist user msg (if identified)<br/>else defer until user_identify fires
        AC->>TS: TurnState::load<br/>(soul, all_users, facts, summaries,<br/>room_ctx, broadcast_targets, tool_schemas)

        loop ≤ MAX_TOOL_ITERS (=6)
            AC->>AC: build_system_prompt<br/>(priority sort + 2000-token cap)
            AC->>LLM: stream(wire_messages, tool_schemas)

            par streaming text
                LLM-->>AC: SSE Text deltas
                AC-->>Ed: TurnEvent::Text(t)
                Ed-->>U: edit_message_text<br/>(≥900ms gap, 429 backoff)
            and accumulating tool calls
                LLM-->>AC: tool_call deltas (by index)
                AC->>AC: assemble in PartialToolCall
            end

            LLM-->>AC: finish_reason → flush ToolCalls + Done

            alt no tool_calls
                AC->>DB: persist assistant msg
                AC-->>TG: return final_text
            else has tool_calls
                AC->>AC: push assistant_with_calls to history
                loop each call (sequential)
                    AC->>T: registry.invoke(name, args, ctx)
                    alt builtin tool
                        T->>DB: read/write
                        T-->>AC: Value
                    else send_message
                        T->>T: rate-limit check (10/min/asker)
                        T->>OB: send_to_principal / send_to_room
                        T->>DB: log outbound msg
                    else MCP tool
                        T->>MCP: tools/call (JSON-RPC, 30s timeout)
                        MCP-->>T: result
                    end
                    AC-->>Ed: TurnEvent::ToolStart / ToolResult
                    AC->>AC: push Message::tool to history
                end
                opt state-mutating tool fired
                    AC->>TS: TurnState::load (reload)
                end
            end
        end

        opt loop exhausted
            AC-->>Ed: emit "(stuck in tool loop...)"
            AC->>DB: persist exhaust note
        end

        AC-->>TG: final_text
        TG->>Ed: drop sink → editor sees rx close → final flush
        opt reply > 4000 chars
            TG-->>U: send overflow chunks
        end
    end
```

---

## State changes within a turn

```mermaid
stateDiagram-v2
    [*] --> Inbound: telegram update arrives
    Inbound --> Filtered: not on allowlist
    Filtered --> [*]: silent

    Inbound --> Logged: group · not addressed
    Logged --> [*]: silent log to room timeline

    Inbound --> TurnStart: addressed or DM
    TurnStart --> ResolvingUser: lookup_by_principal

    ResolvingUser --> KnownUser: principal in DB
    ResolvingUser --> Anonymous: no mapping

    KnownUser --> StateLoaded
    Anonymous --> StateLoaded: user_facts uses shared (user_id=0)

    StateLoaded --> LLMStreaming: wire built · POST /v1/chat/completions

    LLMStreaming --> TextOnly: finish_reason=stop · no tool_calls
    LLMStreaming --> WithTools: finish_reason=tool_calls

    TextOnly --> Persisted: append to messages
    Persisted --> [*]

    WithTools --> ToolDispatch
    ToolDispatch --> StateDirty: user_identify | soul_patch | fact_remember | fact_forget
    ToolDispatch --> StateLoaded: side-effect-only (MCP, send_message, cross_user_transcript)

    StateDirty --> StateLoaded: TurnState::load again

    note right of WithTools
        Looped up to MAX_TOOL_ITERS=6.
        Hard ceiling: surface
        "stuck in tool loop"
        and persist as a real msg.
    end note
```

---

## Background behavior (doesn't block the main reply)

```mermaid
flowchart LR
    A[turn ends] --> B{room mode?}
    B -->|yes| C[already at turn start<br/>refresh_user_vibe<br/>if stale > 5min]
    B -->|no| D[no extra work this turn]

    E[bg compressor tick<br/>every 1800s] --> F{foreach user}
    F --> G{messages.latest_ts<br/>&gt; users.last_summary_ts?}
    G -->|yes| H[refresh_user_vibe<br/>LLM call]
    G -->|no| I[skip]
    F --> J[ensure_yesterday_daily]
    J --> K[cascade_rollup<br/>week → month → quarter]
```

---

## Where each key step lives

| Step | file:line |
|---|---|
| Telegram inbound dispatch | `src/channels/telegram.rs` `handle_message` |
| Addressing gate | `src/channels/telegram.rs` `is_addressed_to_bot` |
| AgentCore turn driver | `src/agent.rs` `run_turn` |
| Personal timeline fetch | `src/messages.rs` `load_personal_timeline` |
| TurnState one-shot load | `src/agent.rs` `TurnState::load` |
| System prompt assembly | `src/agent.rs` `build_system_prompt` |
| LLM SSE parser | `src/llm/openai.rs` `parse_sse` |
| Tool dispatch | `src/tool/mod.rs` `ToolRegistry::invoke` |
| MCP RPC | `src/mcp/mod.rs` `McpClient::call_tool` |
| Streaming edit (Telegram) | `src/channels/telegram.rs` `stream_to_message` |

---

## Why this shape

- **placeholder + edit** instead of "assemble whole reply then send": looks smooth in UX and avoids hand-managing Telegram chunk-by-message — one message_id suffices, only the overflow case sends extra.
- **mpsc + editor task**: `run_turn` uses a sync sink (`&mut FnMut`), but Telegram I/O is async. The channel decouples the two ends.
- **`add_member` fires at the start of a turn**: rooms have "first-speak join" semantics — if you never typed, you're not a member (a privacy feature).
- **MAX_TOOL_ITERS = 6**: empirical. Most turns have 0 or 1 tool calls; occasionally user_identify→fact_remember→reply uses 3. 6 accommodates multi-step fetch + processing without infinite loops.
- **state_dirty flag**: turns the 4-query `TurnState::load` from "run every iter" into "only run after a state-mutating tool", saving 75% of queries in multi-round scenarios like send_message / fetch / cross_user_transcript.
