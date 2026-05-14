//! Background tokio task that periodically runs the compressor for every
//! known user.
//!
//! Why this exists: session-end (`Ctrl+D`) is fine for short CLI sessions,
//! but it leaves two gaps:
//!
//! 1. **Long-running session.** Day rolls over while user is still chatting
//!    → yesterday's daily summary doesn't get written until they finally
//!    quit. By then it's `last_day - 1`.
//! 2. **Always-on processes.** Once we add the Telegram channel (v0.9) the
//!    process never exits. Without a periodic tick, daily summaries
//!    accumulate forever and never get cascade-rolled.
//!
//! The ticker fires every `interval` (configurable, default 1 hour). On
//! each tick: for every user, ensure yesterday's daily exists, then attempt
//! cascade roll-up. All operations are idempotent — repeated firing is a
//! no-op when nothing's changed.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::task::JoinHandle;

use crate::llm::LlmProvider;
use crate::storage::messages;
use crate::storage::summaries;
use crate::storage::users;

/// Spawn the periodic compressor. Returns a handle the caller should `abort()`
/// at shutdown to cancel the task cleanly.
pub fn spawn(
    pool: SqlitePool,
    summarizer_llm: Arc<dyn LlmProvider>,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // First tick fires immediately; skip it so we don't run before the
        // chat session has had a chance to settle.
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;

        loop {
            tick.tick().await;
            run_once(&pool, summarizer_llm.as_ref()).await;
        }
    })
}

async fn run_once(pool: &SqlitePool, llm: &dyn LlmProvider) {
    let users = match users::list_all(pool).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("bg: list users failed: {:#}", e);
            return;
        }
    };
    if users.is_empty() {
        return;
    }
    tracing::debug!("bg: compressor tick — {} user(s)", users.len());

    for user in users {
        // 1. Rolling vibe (users.last_summary). Refresh only if there's been
        // activity since last refresh — avoids burning LLM calls on idle
        // users.
        let needs_vibe = match (user.last_summary_ts, messages::latest_ts(pool, user.id).await) {
            (_, Err(_)) => false,
            (_, Ok(None)) => false,                // no messages at all
            (None, Ok(Some(_))) => true,           // never summarized but has messages
            (Some(ls), Ok(Some(latest))) => latest > ls,
        };
        if needs_vibe {
            match summaries::refresh_user_vibe(pool, llm, user.id, &user.name).await {
                Ok(true) => tracing::info!("bg: vibe refreshed for {}", user.name),
                Ok(false) => {}
                Err(e) => tracing::warn!("bg: vibe refresh for {} failed: {:#}", user.name, e),
            }
        }

        // 2. Yesterday's daily summary (if missing).
        match summaries::ensure_yesterday_daily(pool, llm, user.id, &user.name).await {
            Ok(true) => tracing::info!("bg: daily summary written for {}", user.name),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("bg: daily compressor for {} failed: {:#}", user.name, e);
                continue;
            }
        }

        // 3. Cascade week → month → quarter when source material accumulates.
        match summaries::cascade_rollup(pool, llm, user.id, &user.name).await {
            Ok(rolled) if !rolled.is_empty() => {
                let names: Vec<&str> = rolled.iter().map(|p| p.as_str()).collect();
                tracing::info!("bg: rolled up for {} → {}", user.name, names.join(", "));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("bg: cascade for {} failed: {:#}", user.name, e),
        }
    }
}
