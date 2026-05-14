//! Group-chat addressing logic. Decides whether a message is "to the bot".

use std::time::{Duration, Instant};
use teloxide::types::Message;

use crate::storage::soul::SoulCard;

/// 5-second TTL on the soul-trigger cache (`name` + `nicknames` from the
/// soul card). Group chats can fire many non-addressed messages per minute;
/// without this cache every one of them runs `SoulCard::load`. 5s is short
/// enough that `soul_patch` changes feel near-instant.
const SOUL_TRIGGER_TTL: Duration = Duration::from_secs(5);

/// Cache shared by every Telegram handler in this process.
static SOUL_TRIGGER_CACHE: tokio::sync::OnceCell<
    tokio::sync::Mutex<Option<(String, Vec<String>, Instant)>>,
> = tokio::sync::OnceCell::const_new();

async fn cached_soul_triggers(pool: &sqlx::SqlitePool) -> (String, Vec<String>) {
    let cell = SOUL_TRIGGER_CACHE
        .get_or_init(|| async { tokio::sync::Mutex::new(None) })
        .await;
    {
        let guard = cell.lock().await;
        if let Some((n, nicks, t)) = guard.as_ref() {
            if t.elapsed() < SOUL_TRIGGER_TTL {
                return (n.clone(), nicks.clone());
            }
        }
    }
    if let Ok(soul) = SoulCard::load(pool).await {
        let mut guard = cell.lock().await;
        *guard = Some((soul.name.clone(), soul.nicknames.clone(), Instant::now()));
        return (soul.name, soul.nicknames);
    }
    (String::new(), Vec::new())
}

/// Decide whether a group-chat message is "addressed to the bot".
/// Returns true for any of:
/// - mentions `@bot_username` anywhere in text or caption
/// - is a reply to a message authored by the bot itself
/// - is a slash command (starts with `/`)
/// - text/caption contains any of the bot's soul triggers (name + nicknames),
///   read through `cached_soul_triggers` (5s TTL).
///
/// Note: `msg.text()` and `msg.caption()` are mutually exclusive — a message
/// is either plain text or media-with-caption. We check whichever exists so
/// that `@bot look at this [photo]` triggers the same way pure text does.
pub(super) async fn is_addressed_to_bot(
    pool: &sqlx::SqlitePool,
    msg: &Message,
    bot_username: &str,
) -> bool {
    let body: Option<&str> = msg.text().or_else(|| msg.caption());
    let mention = format!("@{}", bot_username);
    if let Some(t) = body {
        if t.contains(&mention) {
            return true;
        }
        if t.starts_with('/') {
            return true;
        }
    }
    if let Some(reply) = msg.reply_to_message() {
        if let Some(u) = reply.from.as_ref() {
            if u.username.as_deref() == Some(bot_username) {
                return true;
            }
        }
    }
    if let Some(t) = body {
        let (name, nicks) = cached_soul_triggers(pool).await;
        if !name.is_empty() && text_contains_trigger(t, &name) {
            return true;
        }
        for nick in &nicks {
            if !nick.is_empty() && text_contains_trigger(t, nick) {
                return true;
            }
        }
    }
    false
}

/// Match a trigger word inside text. ASCII triggers require a word-boundary
/// match (case-insensitive) so "Folkbot" doesn't fire on "Friday". Non-ASCII
/// triggers (CJK) use plain substring — Chinese has no word boundaries so
/// a CJK nickname matches anywhere it appears, which is what we want.
fn text_contains_trigger(text: &str, trigger: &str) -> bool {
    if trigger.is_empty() {
        return false;
    }
    if trigger.chars().all(|c| c.is_ascii()) {
        let lower = text.to_ascii_lowercase();
        let trig = trigger.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let mut start = 0;
        while let Some(rel) = lower[start..].find(&trig) {
            let abs = start + rel;
            let end = abs + trig.len();
            let prev_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
            let next_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if prev_ok && next_ok {
                return true;
            }
            start = abs + 1;
        }
        false
    } else {
        text.contains(trigger)
    }
}

/// Strip a leading `@bot_username` from text so the LLM doesn't see its
/// own handle as the first token. Trailing/inline mentions are left alone
/// (they're rare and removing them changes meaning).
pub(super) fn strip_leading_mention(text: &str, bot_username: &str) -> String {
    let mention = format!("@{}", bot_username);
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix(&mention) {
        return rest.trim_start().to_string();
    }
    text.to_string()
}
