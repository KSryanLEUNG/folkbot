//! Shared per-asker outbound rate limit.
//!
//! Same instance is shared by `send_message` and `send_file` so a chatty
//! owner doesn't bypass the cap by alternating the two tools. Sliding
//! window of 60 s, max 10 dispatches per window.

use anyhow::{bail, Result};

const SEND_WINDOW_SECS: i64 = 60;
const SEND_MAX_PER_WINDOW: usize = 10;

pub struct SendRateLimit {
    sent_log:
        tokio::sync::Mutex<std::collections::HashMap<i64, std::collections::VecDeque<i64>>>,
}

impl SendRateLimit {
    pub fn new() -> Self {
        Self {
            sent_log: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Check the asker hasn't blown the per-minute cap; record this
    /// dispatch's timestamp on success. Returns `Err` with a
    /// human-readable message on rate-limit hit.
    pub async fn check_and_record(&self, asker_user_id: i64, asker_name: &str) -> Result<()> {
        let now = crate::storage::db::now_ts();
        let mut log = self.sent_log.lock().await;
        let entry = log.entry(asker_user_id).or_default();
        while let Some(&front) = entry.front() {
            if front < now - SEND_WINDOW_SECS {
                entry.pop_front();
            } else {
                break;
            }
        }
        if entry.len() >= SEND_MAX_PER_WINDOW {
            let oldest = *entry.front().unwrap();
            let wait = (oldest + SEND_WINDOW_SECS) - now;
            bail!(
                "rate limit: {} you've used {} send_message/send_file calls in the last {}s, wait {}s before retrying",
                asker_name,
                SEND_MAX_PER_WINDOW,
                SEND_WINDOW_SECS,
                wait.max(1)
            );
        }
        entry.push_back(now);
        Ok(())
    }
}

impl Default for SendRateLimit {
    fn default() -> Self {
        Self::new()
    }
}
