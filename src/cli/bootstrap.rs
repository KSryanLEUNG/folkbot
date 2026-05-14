//! Shared startup wiring for `chat` and `serve`.
//!
//! Both subcommands need the same set: agent core, background compressor,
//! config watcher, MCP-summary stats. Building it twice would be a recipe
//! for drift — keep it here.

use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use sqlx::SqlitePool;

use crate::agent::AgentCore;
use crate::background;
use crate::channels::{self, OutboundChannel};
use crate::config::Config;
use crate::mcp::{McpClient, McpTool};
use crate::tool::builtin::{
    CrossUserTranscript, FactForget, FactRemember, IdentifyUser, SendFile, SendMessage,
    SendRateLimit, SoulPatch,
};
use crate::tool::ToolRegistry;
use crate::watcher;

/// What `bootstrap` returns to whoever launches an `AgentCore` — both
/// `run_chat` and `run_serve` need exactly this set, so we extract it
/// rather than maintaining two near-identical setup blocks.
pub(crate) struct Bootstrap {
    pub(crate) cfg: Config,
    pub(crate) summarizer_llm: Arc<dyn crate::llm::LlmProvider>,
    pub(crate) core: Arc<AgentCore>,
    pub(crate) bg_handle: Option<tokio::task::JoinHandle<()>>,
    pub(crate) watcher_handle: tokio::task::JoinHandle<()>,
    pub(crate) mcp_summary: Vec<String>,
}

/// Build the AgentCore + background compressor + config watcher in one
/// place. Both `run_chat` and `run_serve` start from the same setup.
pub(crate) async fn bootstrap(config_path: &str, pool: &SqlitePool) -> Result<Bootstrap> {
    let cfg = Config::load(config_path)
        .with_context(|| format!("loading {}", config_path))?;

    // MCP filesystem server (and inbound media ingest) both need this to
    // exist before they're invoked. Create eagerly so a fresh checkout
    // works on first run.
    std::fs::create_dir_all(crate::media::WORKSPACE_DIR)
        .with_context(|| format!("creating workspace dir '{}'", crate::media::WORKSPACE_DIR))?;

    let (llm, summarizer_llm) = cfg.build_llms()?;

    let bg_handle = if cfg.agent.compressor.interval_seconds > 0 {
        Some(background::spawn(
            pool.clone(),
            summarizer_llm.clone(),
            std::time::Duration::from_secs(cfg.agent.compressor.interval_seconds),
        ))
    } else {
        None
    };

    let (registry, mcp_summary) = build_registry(&cfg).await?;

    let base_prompt = cfg
        .agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| crate::agent::DEFAULT_BASE_PROMPT.to_string());

    // Audio transcription reuses the main LLM's credentials. Endpoint
    // (whisper-style) is provider-dependent — Poe may not expose it; on
    // failure the channel falls back to a "I can't hear voice yet" marker.
    let audio_creds = match cfg.llm.resolve_api_key() {
        Ok(key) => Some(crate::agent::AudioCreds {
            base_url: cfg.llm.base_url.clone(),
            api_key: key,
            model: "whisper-1".to_string(),
        }),
        Err(_) => None,
    };

    let base_prompt_arc = Arc::new(tokio::sync::RwLock::new(base_prompt));
    let core = Arc::new(AgentCore {
        pool: pool.clone(),
        llm,
        registry,
        base_prompt: base_prompt_arc.clone(),
        outbound: Arc::new(build_outbound(&cfg)),
        audio_creds,
    });

    let watcher_handle = watcher::spawn(
        std::path::PathBuf::from(config_path),
        base_prompt_arc.clone(),
        pool.clone(),
    );

    Ok(Bootstrap {
        cfg,
        summarizer_llm,
        core,
        bg_handle,
        watcher_handle,
        mcp_summary,
    })
}

/// Build the outbound-channel registry. Used by `send_message` so the
/// agent can proactively deliver messages. CLI mode and serve mode both
/// build it the same way (Telegram outbound only sends, doesn't poll —
/// safe to instantiate even in CLI without conflicting with `folkbot serve`).
pub(crate) fn build_outbound(
    cfg: &Config,
) -> std::collections::HashMap<String, Arc<dyn OutboundChannel>> {
    let mut map: std::collections::HashMap<String, Arc<dyn OutboundChannel>> =
        std::collections::HashMap::new();
    if let Some(tg) = &cfg.channels.telegram {
        match tg.resolve_token() {
            Ok(token) => {
                map.insert(
                    "telegram".into(),
                    Arc::new(channels::telegram::TelegramOutbound::from_token(token)),
                );
            }
            Err(e) => {
                eprintln!(
                    "{} telegram outbound disabled: {}",
                    "!".yellow().bold(),
                    e
                );
            }
        }
    }
    map
}

/// Build the agent's tool registry: built-ins + MCP servers from config.
pub(crate) async fn build_registry(cfg: &Config) -> Result<(ToolRegistry, Vec<String>)> {
    let mut registry = ToolRegistry::new();
    // SendMessage and SendFile share one rate limiter so alternating
    // between text + file dispatches still hits the same 10/min cap.
    let send_rate = Arc::new(SendRateLimit::new());
    registry.register(Arc::new(IdentifyUser));
    registry.register(Arc::new(FactRemember));
    registry.register(Arc::new(FactForget));
    registry.register(Arc::new(SoulPatch));
    registry.register(Arc::new(CrossUserTranscript));
    registry.register(Arc::new(SendMessage::new(send_rate.clone())));
    registry.register(Arc::new(SendFile::new(send_rate)));

    let mut mcp_summary = Vec::new();
    for srv_cfg in &cfg.mcp.servers {
        match McpClient::spawn(srv_cfg).await {
            Ok(client) => match client.list_tools().await {
                Ok(tools) => {
                    let n = tools.len();
                    for desc in tools {
                        let tool = McpTool::new(srv_cfg.name.clone(), desc, client.clone());
                        let exposed = tool.exposed_name();
                        registry.register(Arc::new(tool));
                        tracing::info!("MCP[{}] tool registered: {}", srv_cfg.name, exposed);
                    }
                    mcp_summary.push(format!("{}({})", srv_cfg.name, n));
                }
                Err(e) => eprintln!(
                    "{} failed to list MCP[{}] tools: {:#}",
                    "✗".red().bold(),
                    srv_cfg.name,
                    e
                ),
            },
            Err(e) => eprintln!(
                "{} failed to spawn MCP[{}]: {:#}",
                "✗".red().bold(),
                srv_cfg.name,
                e
            ),
        }
    }
    Ok((registry, mcp_summary))
}
