//! Polling config watcher.
//!
//! Hot-reload `[agent.system_prompt]` when folkbot.toml is edited mid-session.
//! Polling instead of `notify` because:
//!   - polling is cross-platform with no extra deps
//!   - 5s latency is fine for "I tweaked the persona, want it to take effect"
//!   - atomic file replaces (vim default, `mv tmp folkbot.toml`) are detected via
//!     mtime regardless
//!
//! Only the prompt is hot-reloadable — provider / model / channel changes
//! still require a restart. Hot-swapping HTTP clients mid-flight is a v1.0+
//! concern.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::agent::DEFAULT_BASE_PROMPT;
use crate::config::Config;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub fn spawn(
    config_path: PathBuf,
    base_prompt: Arc<RwLock<String>>,
    _pool: SqlitePool, // reserved for future per-DB reactions
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_mtime = read_mtime(&config_path);
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let mtime = read_mtime(&config_path);
            if mtime.is_none() || mtime == last_mtime {
                continue;
            }
            last_mtime = mtime;
            match Config::load(&config_path) {
                Ok(cfg) => {
                    let new_prompt = cfg
                        .agent
                        .system_prompt
                        .unwrap_or_else(|| DEFAULT_BASE_PROMPT.to_string());
                    let mut guard = base_prompt.write().await;
                    if *guard != new_prompt {
                        tracing::info!(
                            "hot-reload: system_prompt changed ({} → {} chars)",
                            guard.chars().count(),
                            new_prompt.chars().count()
                        );
                        *guard = new_prompt;
                    }
                }
                Err(e) => {
                    tracing::warn!("hot-reload: parse failed, keeping old config: {:#}", e);
                }
            }
        }
    })
}

fn read_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}
