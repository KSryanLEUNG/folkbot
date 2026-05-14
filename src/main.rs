//! Folkbot agent — binary entry point.
//!
//! Real work lives in the submodules:
//!   - `cli`     — command-line subcommands (chat / serve / soul / facts / ...)
//!   - `agent`   — orchestrator + run_turn
//!   - `channels`— inbound/outbound adapters (telegram)
//!   - `storage` — SQL persistence layer
//!   - `tool`    — built-in + MCP tool registry
//!   - `llm`     — provider clients
//!   - `mcp`     — MCP subprocess spawn + JSON-RPC

mod agent;
mod background;
mod channels;
mod cli;
mod config;
mod llm;
mod mcp;
mod media;
mod slash;
mod storage;
mod tokens;
mod tool;
mod util;
mod watcher;

use anyhow::Result;

const DB_PATH: &str = "data/folkbot.db";

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let pool = storage::db::init_pool(DB_PATH).await?;
    cli::run(&pool).await
}
