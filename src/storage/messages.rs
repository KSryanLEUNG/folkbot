//! Persistent message log, scoped per user (DM) or per room (v1.2).
//!
//! DM messages live under `conversation_id = 0` (the dedicated DM sentinel)
//! and are tagged with `user_id` so each human's 1:1 chat history is
//! isolated. v0.4 made cross-user privacy structural for DMs: no API to
//! list user B's raw messages.
//!
//! v1.2 adds rooms: rows in `conversations` with `kind='room'`. Their ids
//! come from `INTEGER PRIMARY KEY AUTOINCREMENT` (so the first room got
//! id=1). Messages from rooms carry `conversation_id` matching that row.
//!
//! ## v1.2.1 sentinel migration
//!
//! Originally DM hard-coded `CONVERSATION_ID = 1`, which collided with the
//! first auto-incremented room id. We moved DM to `0` going forward so DM
//! and room namespaces don't overlap. Legacy rows at `conversation_id = 1`
//! are LEFT IN PLACE — they're a mix of pre-room DM and post-room group
//! activity that can't be reliably separated, so they all read out as the
//! id=1 room from now on. Practical effect: each user's DM history before
//! the migration is invisible from the DM sliding window. Acceptable
//! trade-off for testing-phase data.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::storage::db::now_ts;
use crate::llm::{Message, Role};

/// DM sentinel — every DM message has `conversation_id = 0`. Rooms use
/// their own auto-incremented id (>=1) and are queried separately.
const CONVERSATION_ID: i64 = 0;

pub async fn append(pool: &SqlitePool, user_id: i64, role: Role, content: &str) -> Result<i64> {
    let role_str = match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        // System / Tool / assistant-with-tool-calls don't go in the log; they
        // either come from config (system) or live only within a turn.
        _ => return Ok(0),
    };
    let res = sqlx::query(
        "INSERT INTO messages (ts, role, content, conversation_id, user_id) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(now_ts())
    .bind(role_str)
    .bind(content)
    .bind(CONVERSATION_ID)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Load the most recent `n` messages for a given user, in chronological
/// order. Returns `(Message, ts)` pairs so callers can render absolute
/// timestamps to the LLM (the agent has no concept of "now" without them).
pub async fn load_last_n(pool: &SqlitePool, user_id: i64, n: usize) -> Result<Vec<(Message, i64)>> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT role, content, ts FROM (
             SELECT id, ts, role, content FROM messages
             WHERE conversation_id = ? AND user_id = ?
             ORDER BY ts DESC, id DESC LIMIT ?
         ) AS recent ORDER BY ts ASC, id ASC",
    )
    .bind(CONVERSATION_ID)
    .bind(user_id)
    .bind(n as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(role, content, ts)| match role.as_str() {
            "user" => Some((Message::user(content), ts)),
            "assistant" => Some((Message::assistant(content), ts)),
            _ => None,
        })
        .collect())
}

/// Latest message timestamp for a user, or None if they have no messages.
/// Used by the background compressor to decide whether to refresh
/// `users.last_summary` (only if there's been activity since last refresh).
///
/// v1.2: drops the `conversation_id` filter so room activity also counts —
/// otherwise a user who only chats in group rooms would never get their
/// per-user summary refreshed.
pub async fn latest_ts(pool: &SqlitePool, user_id: i64) -> Result<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT MAX(ts) FROM messages WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(ts,)| ts).filter(|&ts| ts > 0))
}

/// Append a message to a room (v1.2). `speaker_id` is the user who said it
/// (or who triggered an assistant reply); `conv_id` is the room id.
pub async fn append_room(
    pool: &SqlitePool,
    conv_id: i64,
    speaker_id: i64,
    role: Role,
    content: &str,
) -> Result<i64> {
    let role_str = match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        _ => return Ok(0),
    };
    let res = sqlx::query(
        "INSERT INTO messages (ts, role, content, conversation_id, user_id) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(now_ts())
    .bind(role_str)
    .bind(content)
    .bind(conv_id)
    .bind(speaker_id)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Load the most recent `n` messages a user has sent across ALL
/// conversations (DM + every room they speak in). Returns
/// `(Message, ts, conv_id)` triples so callers can tag each line with
/// the originating conversation. Used by `refresh_user_vibe` so the
/// per-user summary spans both DM and group activity.
pub async fn load_last_n_for_user_all_convs(
    pool: &SqlitePool,
    user_id: i64,
    n: usize,
) -> Result<Vec<(Message, i64, i64)>> {
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT role, content, ts, conversation_id FROM (
             SELECT id, ts, role, content, conversation_id FROM messages
             WHERE user_id = ?
             ORDER BY ts DESC, id DESC LIMIT ?
         ) AS recent ORDER BY ts ASC, id ASC",
    )
    .bind(user_id)
    .bind(n as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(role, content, ts, cid)| match role.as_str() {
            "user" => Some((Message::user(content), ts, cid)),
            "assistant" => Some((Message::assistant(content), ts, cid)),
            _ => None,
        })
        .collect())
}

/// Load the speaker's "personal timeline" (v1.2.1): the union of their own
/// DM messages and all messages in rooms they're a member of, ordered
/// chronologically and capped at `n`. Returns
/// `(Message, ts, conv_id, speaker_id_of_row)` quadruples so the agent
/// can render `[ts | source] speaker: body` prefixes.
///
/// Privacy: only this speaker's DM is included (their own private 1:1).
/// Other users' DM is NOT pulled in — owner uses `cross_user_transcript`
/// for raw cross-DM access; vice_owner only sees concept-level summary.
/// Rooms are public-hall by design, so all room participants' messages
/// are included.
pub async fn load_personal_timeline(
    pool: &SqlitePool,
    user_id: i64,
    n: usize,
) -> Result<Vec<(Message, i64, i64, i64)>> {
    let rows: Vec<(String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT role, content, ts, conversation_id, user_id FROM (
             SELECT id, ts, role, content, conversation_id, user_id FROM messages
             WHERE (conversation_id = 0 AND user_id = ?)
                OR conversation_id IN (
                    SELECT conv_id FROM conv_members WHERE user_id = ?
                )
             ORDER BY ts DESC, id DESC LIMIT ?
         ) AS recent ORDER BY ts ASC, id ASC",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(n as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(role, content, ts, cid, sid)| match role.as_str() {
            "user" => Some((Message::user(content), ts, cid, sid)),
            "assistant" => Some((Message::assistant(content), ts, cid, sid)),
            _ => None,
        })
        .collect())
}

/// Total message count for a user (or whole conversation if user_id is None).
pub async fn count(pool: &SqlitePool, user_id: Option<i64>) -> Result<i64> {
    let (n,): (i64,) = match user_id {
        Some(uid) => {
            sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND user_id = ?")
                .bind(CONVERSATION_ID)
                .bind(uid)
                .fetch_one(pool)
                .await?
        }
        None => sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
            .bind(CONVERSATION_ID)
            .fetch_one(pool)
            .await?,
    };
    Ok(n)
}

/// Hard delete all persisted messages for a user (or all users if None).
pub async fn clear(pool: &SqlitePool, user_id: Option<i64>) -> Result<u64> {
    let res = match user_id {
        Some(uid) => sqlx::query("DELETE FROM messages WHERE conversation_id = ? AND user_id = ?")
            .bind(CONVERSATION_ID)
            .bind(uid)
            .execute(pool)
            .await?,
        None => sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
            .bind(CONVERSATION_ID)
            .execute(pool)
            .await?,
    };
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn personal_timeline_excludes_other_users_dm() {
        let pool = crate::storage::db::test_pool().await;
        // Two users, only DM activity for each (no rooms).
        append(&pool, 100, Role::User, "alice secret").await.unwrap();
        append(&pool, 100, Role::Assistant, "ack alice").await.unwrap();
        append(&pool, 200, Role::User, "bob secret").await.unwrap();
        append(&pool, 200, Role::Assistant, "ack bob").await.unwrap();

        // Speaker = alice (user_id 100). Should see her DM only — never bob's.
        let timeline = load_personal_timeline(&pool, 100, 50).await.unwrap();
        let bodies: Vec<String> = timeline
            .iter()
            .map(|(m, _, _, _)| m.content.as_ref().map(|c| c.as_text()).unwrap_or_default())
            .collect();
        assert!(bodies.iter().any(|b| b.contains("alice")));
        assert!(
            !bodies.iter().any(|b| b.contains("bob")),
            "bob's DM leaked into alice's personal timeline: {:?}",
            bodies
        );
    }
}
