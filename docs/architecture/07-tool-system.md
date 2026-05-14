# 07 · Tool System — tool registration, dispatch, MCP integration

The LLM calls tools via the OpenAI tool-call mechanism. Built-in and MCP — the two sources — are unified through one `Tool` trait + `ToolRegistry`.

## Overall class relationships

```mermaid
classDiagram
    class Tool {
        <<trait>>
        +schema() ToolSchema
        +invoke(args, ctx) Result~Value~
    }

    class ToolRegistry {
        -tools: HashMap~String, Arc~dyn Tool~~
        +register(tool: Arc~dyn Tool~)
        +schemas() Vec~ToolSchema~
        +names() Vec~String~
        +invoke(name, args, ctx) Result~Value~
    }

    class ToolContext {
        +pool: SqlitePool
        +principal: Principal
        +user_id: Option~i64~
        +outbound: Arc~HashMap~String, dyn OutboundChannel~~
    }

    class ToolSchema {
        +name: String
        +description: String
        +parameters: serde_json::Value
    }

    class IdentifyUser {
        +invoke()
    }
    class FactRemember {
        +invoke()
    }
    class FactForget {
        +invoke()
    }
    class SoulPatch {
        +invoke()
    }
    class CrossUserTranscript {
        +invoke()
    }
    class SendMessage {
        -sent_log: Mutex~HashMap~i64, VecDeque~i64~~~
        +new() Self
        +invoke()
    }
    class McpTool {
        -server: String
        -inner_name: String
        -descriptor: McpToolDescriptor
        -client: Arc~McpClient~
        +exposed_name() String
        +invoke()
    }

    Tool <|.. IdentifyUser
    Tool <|.. FactRemember
    Tool <|.. FactForget
    Tool <|.. SoulPatch
    Tool <|.. CrossUserTranscript
    Tool <|.. SendMessage
    Tool <|.. McpTool

    ToolRegistry "1" o-- "*" Tool
    Tool ..> ToolContext : uses
    Tool ..> ToolSchema : produces

    class McpClient {
        -name: String
        -next_id: AtomicU64
        -pending: Arc~Mutex~HashMap~u64, oneshot::Sender~~~
        -writer_tx: mpsc::Sender~Vec~u8~~
        -_child: Mutex~Child~
        +spawn(cfg) Arc~Self~
        +list_tools() Vec~McpToolDescriptor~
        +call_tool(name, args) Value
    }

    McpTool --> McpClient : Arc
```

---

## Built-in vs MCP

```mermaid
flowchart LR
    classDef builtin fill:#fce7f3,stroke:#9f1239
    classDef mcp fill:#dbeafe,stroke:#1e40af
    classDef child fill:#e5e7eb,stroke:#374151

    subgraph In[in-process · Rust]
        IU[IdentifyUser]:::builtin
        FR[FactRemember]:::builtin
        FF[FactForget]:::builtin
        SP[SoulPatch]:::builtin
        CT[CrossUserTranscript]:::builtin
        SM[SendMessage]:::builtin
    end

    subgraph Out[child process · any language]
        FetchT[fetch_fetch]:::mcp
        FsR[fs_read_file]:::mcp
        FsW[fs_write_file]:::mcp
        FsE[fs_edit_file]:::mcp
        FsL[fs_list_directory]:::mcp
        FsM[fs_move_file]:::mcp
        FsS[fs_search_files]:::mcp
        Etc[...]:::mcp
    end

    subgraph Wrap[Rust-side wrapper]
        McpT1[McpTool 'fetch']
        McpT2[McpTool 'fs']
    end

    FetchT --> McpT1
    FsR --> McpT2
    FsW --> McpT2
    FsE --> McpT2
    FsL --> McpT2
    FsM --> McpT2
    FsS --> McpT2
    Etc --> McpT2

    Reg[(ToolRegistry)]
    IU --> Reg
    FR --> Reg
    FF --> Reg
    SP --> Reg
    CT --> Reg
    SM --> Reg
    McpT1 --> Reg
    McpT2 --> Reg

    Reg --> LLM[LLM (via OpenAI tools field)]

    Child1[(uvx mcp-server-fetch<br/>Python)]:::child
    Child2[(npx server-filesystem<br/>Node)]:::child
    McpT1 -.->|JSON-RPC over stdio| Child1
    McpT2 -.->|JSON-RPC over stdio| Child2
```

**To the LLM there's no difference**: both kinds look the same (`name`, `description`, `parameters`). Only the dispatch path differs.

---

## MCP startup handshake

```mermaid
sequenceDiagram
    autonumber
    participant App as build_registry
    participant C as McpClient::spawn
    participant W as writer task
    participant R as reader task
    participant Child as MCP child<br/>(uvx mcp-server-fetch)

    App->>C: spawn(McpServerConfig{name, command, args, env})
    C->>Child: Command::spawn (stdin/stdout piped, kill_on_drop)
    C->>W: tokio::spawn(writer)<br/>drains mpsc → child.stdin + \n
    C->>R: tokio::spawn(reader)<br/>BufReader.lines() → match id → pending.send

    Note over C,Child: handshake
    C->>W: request 'initialize'<br/>{protocolVersion, capabilities, clientInfo}
    W->>Child: stdin write
    Child-->>R: stdout line {id:1, result:{...}}
    R-->>C: oneshot resolved → result returned

    C->>W: notify 'notifications/initialized' {}
    W->>Child: stdin write
    Note right of Child: Standard MCP flow: initialize → initialized → ready

    App->>C: list_tools()
    C->>W: request 'tools/list' {}
    W->>Child: stdin write
    Child-->>R: stdout line {id:2, result:{tools:[...]}}
    R-->>C: descriptors

    Note over App: foreach descriptor → wrap McpTool → registry.register
```

If `initialize` doesn't respond within 30 seconds it times out (`McpClient::request`); startup fails, prints an error, but doesn't kill the whole process.

---

## Tool dispatch details

```mermaid
flowchart TB
    classDef builtin fill:#fce7f3,stroke:#9f1239
    classDef mcp fill:#dbeafe,stroke:#1e40af
    classDef state fill:#fef9c3,stroke:#854d0e

    Start[LLM returns finish_reason=tool_calls<br/>SSE stream ends]
    Start --> Assemble[parse_sse already assembled<br/>Vec_PartialToolCall_ → Vec_ToolCall_]
    Assemble --> Push[push assistant_with_calls into history]
    Push --> ForEach{foreach call · sequential}

    ForEach --> ParseArgs[serde_json::from_str call.function.arguments<br/>→ Value · on failure → empty Object]
    ParseArgs --> EmitStart[on_event TurnEvent::ToolStart]
    EmitStart --> BuildCtx[ToolContext{pool, principal, user_id, outbound}]
    BuildCtx --> Invoke[registry.invoke name args ctx]

    Invoke --> Branch{tool kind?}
    Branch -->|builtin| BuiltDispatch[call .invoke method directly]:::builtin
    Branch -->|McpTool| McpDispatch[McpClient::call_tool name args<br/>· via mpsc → child]:::mcp

    BuiltDispatch --> Result
    McpDispatch --> Result
    Result[Value · or wrapped as 'error']

    Result --> EmitResult[on_event TurnEvent::ToolResult]
    EmitResult --> PushTool[push Message::tool call.id json]
    PushTool --> CheckDirty{call.name in<br/>state-mutating list?}:::state
    CheckDirty -->|yes · user_identify/soul_patch/<br/>fact_remember/fact_forget| Mark[state_dirty = true]
    CheckDirty -->|no| Continue1[continue]

    Mark --> Continue1
    Continue1 --> NextCall{more calls?}
    NextCall -->|yes| ForEach
    NextCall -->|no| AfterLoop

    AfterLoop --> ReloadChk{state_dirty?}
    ReloadChk -->|yes| Reload[TurnState::load again]
    ReloadChk -->|no| Skip[skip reload]

    Reload --> NextIter[continue agent for_loop · iter+1]
    Skip --> NextIter
```

---

## Tool naming rules

```mermaid
flowchart LR
    Builtin[Built-in<br/>fixed names]
    Mcp[MCP<br/>name = server-name + '_' + tool-name<br/>non ASCII alphanumeric/_/- → '_']

    Builtin --> Names[user_identify<br/>fact_remember<br/>fact_forget<br/>soul_patch<br/>cross_user_transcript<br/>send_message]

    Mcp --> McpNames[fetch_fetch<br/>fs_read_file<br/>fs_write_file<br/>fs_edit_file<br/>fs_list_directory<br/>fs_search_files<br/>fs_move_file<br/>fs_get_file_info<br/>fs_create_directory<br/>fs_list_allowed_directories]

    Limit["OpenAI restriction:<br/>^[a-zA-Z0-9_-]{1,64}$<br/>over 64 → truncate<br/>illegal char → '_'"]

    Names --> Limit
    McpNames --> Limit
```

Collision avoidance: every MCP tool carries the `<server>_` prefix; built-ins are snake_case verbs — naturally separated.

---

## Failure handling

```mermaid
flowchart TB
    Invoke[registry.invoke]
    Invoke --> R{result?}

    R -->|Ok Value| Pass[returned to LLM as-is]
    R -->|Err e| Wrap["json! 'error': e.to_string"]

    Wrap --> ToLLM[as Message::tool content<br/>LLM, on seeing it, typically tells the user honestly<br/>or adjusts strategy and retries]

    Pass --> ToLLM

    note1[every tool error is logged<br/>tracing::warn in telegram handler]
    note2[Built-in failure = bail! with reason<br/>MCP failure = JSON-RPC error / timeout]
    note3[no retry / circuit breaker<br/>·next milestone·]
```

---

## Why it's designed this way

- **One trait wrapping in-process + child process**: future grpc / wasm / wasi tools only need to implement `Tool`; the agent stays the same.
- **MCP uses stdio JSON-RPC**: that's the MCP standard, and servers can be shared with other clients like Claude Desktop.
- **`SendMessage` carries an in-memory rate limit**: state lives with the tool instance (singleton in the registry); only a restart resets it — sufficient for household use, no need to persist to DB.
- **state_dirty flag**: avoids reloading state for every tool (wasted work) while guaranteeing that the next round after a state-mutating tool is fresh.
- **Tool name prefix + sanitize**: in the future, if an MCP server name contains special characters like `.`, they'll be auto-converted to `_`; no manual mapping required.
