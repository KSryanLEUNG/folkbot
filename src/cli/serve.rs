//! `folkbot serve` — long-lived daemon that hosts channel adapters.

use anyhow::{Context, Result};
use colored::Colorize;
use sqlx::SqlitePool;

use super::bootstrap::{bootstrap, Bootstrap};
use crate::channels;

pub(crate) async fn run(config_path: &str, pool: &SqlitePool) -> Result<()> {
    let Bootstrap {
        cfg,
        summarizer_llm: _,
        core,
        bg_handle,
        watcher_handle,
        mcp_summary,
    } = bootstrap(config_path, pool).await?;

    println!(
        "{} v{} serve mode  ({} / {})",
        "Folkbot".bright_cyan().bold(),
        env!("CARGO_PKG_VERSION"),
        cfg.llm.provider.dimmed(),
        cfg.llm.model.dimmed(),
    );
    if !mcp_summary.is_empty() {
        println!("  {}", format!("mcp: {}", mcp_summary.join(", ")).dimmed());
    }

    let mut channel_handles = Vec::new();
    if let Some(tg_cfg) = cfg.channels.telegram.clone() {
        match channels::telegram::spawn(core.clone(), tg_cfg).await {
            Ok(h) => channel_handles.push(("telegram", h)),
            Err(e) => eprintln!("{} telegram channel: {:#}", "✗".red().bold(), e),
        }
    }

    if channel_handles.is_empty() {
        if let Some(h) = bg_handle {
            h.abort();
        }
        anyhow::bail!("no channels configured — add `[channels.telegram]` to folkbot.toml");
    }

    println!("{}", "  Ctrl+C to stop".dimmed());

    tokio::signal::ctrl_c()
        .await
        .context("install Ctrl+C handler")?;

    println!("\n{} shutting down channels...", "·".dimmed());
    if let Some(h) = bg_handle {
        h.abort();
    }
    watcher_handle.abort();
    for (name, h) in channel_handles {
        h.abort();
        tracing::info!("stopped channel: {}", name);
    }
    Ok(())
}
