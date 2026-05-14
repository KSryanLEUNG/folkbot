//! Streaming-edit helpers for Telegram replies.
//!
//! `stream_to_message` drains text deltas off an mpsc and edits a placeholder
//! Telegram message in-place every ~900 ms. `chunk_for_telegram` splits
//! over-budget replies into 4096-char chunks for sending as separate messages.

use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId};
use tokio::sync::mpsc;

pub(super) const TG_LIMIT: usize = 4096;
pub(super) const SAFE_EDIT_LIMIT: usize = 4000; // leave headroom for "…" suffix
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(900);

/// Drain text deltas from `rx`, edit Telegram message every ~900ms with the
/// accumulated text. Final flush when `rx` closes. Caps display at
/// `SAFE_EDIT_LIMIT` chars; overflow is sent as new messages by the caller.
pub(super) async fn stream_to_message(
    bot: Bot,
    chat_id: ChatId,
    message_id: MessageId,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    let mut buf = String::new();
    let mut last_displayed = String::new();
    let mut last_edit = Instant::now() - MIN_EDIT_INTERVAL; // first edit eligible immediately

    while let Some(t) = rx.recv().await {
        buf.push_str(&t);
        if last_edit.elapsed() < MIN_EDIT_INTERVAL {
            continue;
        }
        let display = render_for_telegram(&buf);
        if display == last_displayed || display.is_empty() {
            continue;
        }
        match bot.edit_message_text(chat_id, message_id, &display).await {
            Ok(_) => {
                last_displayed = display;
                last_edit = Instant::now();
            }
            Err(e) => {
                tracing::debug!("telegram edit deferred: {}", e);
                // Likely 429. Back off — let next tick retry.
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    // Final flush — best effort, retry once on rate-limit.
    let display = render_for_telegram(&buf);
    if display != last_displayed && !display.is_empty() {
        if bot
            .edit_message_text(chat_id, message_id, &display)
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = bot.edit_message_text(chat_id, message_id, display).await;
        }
    }
}

fn render_for_telegram(buf: &str) -> String {
    let count = buf.chars().count();
    if count <= SAFE_EDIT_LIMIT {
        buf.to_string()
    } else {
        let truncated: String = buf.chars().take(SAFE_EDIT_LIMIT).collect();
        format!("{}…", truncated)
    }
}

/// Telegram caps single messages at 4096 chars. Split on paragraph boundaries
/// when possible; if a single "line" still exceeds the cap (CJK without
/// newlines is the realistic case — LLM often dumps a paragraph as one long
/// run), fall back to a hard char-boundary split. Never produce a chunk
/// larger than `TG_LIMIT - 50`.
pub(super) fn chunk_for_telegram(text: &str) -> Vec<String> {
    let cap = TG_LIMIT - 50;
    if text.chars().count() <= cap {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in text.split_inclusive('\n') {
        let line_len = line.chars().count();
        // Single oversize line: flush current buf, then hard-split this line.
        if line_len > cap {
            if !buf.is_empty() {
                out.push(std::mem::take(&mut buf));
            }
            out.extend(hard_split(line, cap));
            continue;
        }
        if buf.chars().count() + line_len > cap && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
        }
        buf.push_str(line);
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Char-boundary split. Used as the last resort when no paragraph boundary
/// fits in the budget.
fn hard_split(text: &str, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut count = 0;
    for ch in text.chars() {
        if count == cap {
            out.push(std::mem::take(&mut buf));
            count = 0;
        }
        buf.push(ch);
        count += 1;
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}
