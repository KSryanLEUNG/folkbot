//! `folkbot prompt` — render the composed system prompt for a user (debug aid).

use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use sqlx::SqlitePool;

use crate::agent::AgentCore;
use crate::config::Config;
use crate::storage::users::{self, Principal};
use crate::tool::ToolRegistry;

pub(crate) async fn run(
    user: Option<&str>,
    conv_id: Option<i64>,
    config_path: &str,
    pool: &SqlitePool,
) -> Result<()> {
    let cfg = Config::load(config_path)
        .with_context(|| format!("loading {}", config_path))?;
    let (llm, _summarizer) = cfg.build_llms()?;
    let base_prompt = cfg
        .agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| crate::agent::DEFAULT_BASE_PROMPT.to_string());

    // We don't need the full registry (would spawn MCP), and we don't need
    // the summarizer LLM. Just enough to compose.
    let core = AgentCore {
        pool: pool.clone(),
        llm,
        registry: ToolRegistry::new(),
        base_prompt: Arc::new(tokio::sync::RwLock::new(base_prompt)),
        outbound: Arc::new(std::collections::HashMap::new()),
        audio_creds: None,
    };

    let user_id = match user {
        Some(name) => users::lookup_by_name(pool, name)
            .await?
            .map(|u| u.id),
        None => {
            let principal = Principal::cli_local();
            users::lookup_by_principal(pool, &principal)
                .await?
                .map(|u| u.id)
        }
    };

    let label = match (user_id, conv_id) {
        (Some(uid), Some(cid)) => format!("user_id={} · conv_id={}", uid, cid),
        (Some(uid), None) => format!("user_id={}", uid),
        (None, Some(cid)) => format!("unidentified · conv_id={}", cid),
        (None, None) => "unidentified".to_string(),
    };

    let prompt = core.compose_system_prompt_for(user_id, conv_id).await?;
    let token_count = crate::tokens::count(&prompt);

    println!(
        "{} {} {}",
        "── system prompt for".bright_cyan().bold(),
        label.bright_yellow(),
        format!("({} tokens)", token_count).dimmed()
    );
    println!("{}", "─".repeat(60).dimmed());
    println!("{}", prompt);
    println!("{}", "─".repeat(60).dimmed());
    Ok(())
}
