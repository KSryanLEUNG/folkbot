//! Conversation rooms (v1.2).
//!
//! A "conversation" here means a shared message space. DM 1:1 chats keep
//! the legacy `conversation_id = 1` sentinel and are not tracked in this
//! table. Rooms (Telegram group chats, future Discord channels, …) get
//! their own row keyed by `(channel, room_key)` so multiple group chats
//! on the same channel don't collide.
//!
//! Membership is recorded lazily: a user becomes a member the first time
//! they speak in the room. `list_members` is what the agent uses to fill
//! "who is in this room" in the system prompt.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::storage::db::now_ts;
use crate::storage::users::{self, User};

/// Look up a room by `(channel, room_key)` or create it.
/// `label` is the human-readable name (e.g. Telegram group title); when
/// looking up an existing room we update the label so it stays fresh.
pub async fn lookup_or_create_room(
    pool: &SqlitePool,
    channel: &str,
    room_key: &str,
    label: Option<&str>,
) -> Result<i64> {
    if let Some((id,)) = sqlx::query_as::<_, (i64,)>(
        "SELECT id FROM conversations WHERE channel = ? AND room_key = ?",
    )
    .bind(channel)
    .bind(room_key)
    .fetch_optional(pool)
    .await?
    {
        if let Some(label) = label {
            let _ = sqlx::query("UPDATE conversations SET label = ? WHERE id = ?")
                .bind(label)
                .bind(id)
                .execute(pool)
                .await;
        }
        return Ok(id);
    }
    let res = sqlx::query(
        "INSERT INTO conversations (kind, channel, room_key, label, created_at) \
         VALUES ('room', ?, ?, ?, ?)",
    )
    .bind(channel)
    .bind(room_key)
    .bind(label)
    .bind(now_ts())
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Idempotent member registration. No-op if already a member.
pub async fn add_member(pool: &SqlitePool, conv_id: i64, user_id: i64) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO conv_members (conv_id, user_id, joined_at) VALUES (?, ?, ?)",
    )
    .bind(conv_id)
    .bind(user_id)
    .bind(now_ts())
    .execute(pool)
    .await?;
    Ok(())
}

/// All current members of a room as full `User` records, in joined-at order.
///
/// One JOIN query — was N+1 before (one lookup per member).
pub async fn list_members(pool: &SqlitePool, conv_id: i64) -> Result<Vec<User>> {
    // Resolve in-batch via lookup_many_by_ids, then re-sort by joined_at.
    let ids: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT user_id, joined_at FROM conv_members
         WHERE conv_id = ? ORDER BY joined_at ASC",
    )
    .bind(conv_id)
    .fetch_all(pool)
    .await?;
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let just_ids: Vec<i64> = ids.iter().map(|(u, _)| *u).collect();
    let mut by_id = users::lookup_many_by_ids(pool, &just_ids).await?;
    let mut out = Vec::with_capacity(ids.len());
    for (uid, _) in ids {
        if let Some(u) = by_id.remove(&uid) {
            out.push(u);
        }
    }
    Ok(out)
}

/// Bulk-fetch labels for a set of conv_ids. Used by the personal-timeline
/// renderer in agent.rs, which sees a mix of conv_ids per turn and needs
/// each one's label in one go. DM (conv_id=0) is mapped to `None` since
/// it has no row in `conversations`.
///
/// One IN-clause query — was N+1 before.
pub async fn labels_for(
    pool: &SqlitePool,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, Option<String>>> {
    let mut out = std::collections::HashMap::new();
    let unique: std::collections::HashSet<i64> = ids.iter().copied().collect();
    if unique.is_empty() {
        return Ok(out);
    }
    // DM sentinel always maps to None — handled separately so we don't
    // include it in the IN clause (it won't match a row anyway, but
    // cleaner to short-circuit).
    let mut to_fetch: Vec<i64> = Vec::with_capacity(unique.len());
    for &cid in &unique {
        if cid == 0 {
            out.insert(0, None);
        } else {
            to_fetch.push(cid);
        }
    }
    if to_fetch.is_empty() {
        return Ok(out);
    }
    let placeholders: Vec<&str> = (0..to_fetch.len()).map(|_| "?").collect();
    let sql = format!(
        "SELECT id, label FROM conversations WHERE id IN ({})",
        placeholders.join(",")
    );
    let mut q = sqlx::query_as::<_, (i64, Option<String>)>(&sql);
    for cid in &to_fetch {
        q = q.bind(cid);
    }
    let rows = q.fetch_all(pool).await?;
    for (cid, label) in rows {
        out.insert(cid, label);
    }
    // Any conv_id with no row maps to None too (silently — caller can
    // always render `group#<id>` from the absent label).
    for cid in to_fetch {
        out.entry(cid).or_insert(None);
    }
    Ok(out)
}

/// Room label for prompt rendering (e.g. "family group"). None if not set.
pub async fn label(pool: &SqlitePool, conv_id: i64) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT label FROM conversations WHERE id = ?",
    )
    .bind(conv_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(l,)| l))
}

/// Resolve a room by user-provided label (case-insensitive), restricted to
/// rooms the asker is a member of. Returns `(conv_id, channel, room_key)`.
/// Returns `Ok(None)` for "no match"; errors only on DB issues or ambiguous
/// matches (multiple rooms with the same label that the user is in).
pub async fn find_room_by_label_for_user(
    pool: &SqlitePool,
    user_id: i64,
    label_query: &str,
) -> Result<Option<(i64, String, String)>> {
    let rooms = rooms_for_user(pool, user_id).await?;
    let q_lower = label_query.to_lowercase();
    let mut matches: Vec<i64> = rooms
        .into_iter()
        .filter_map(|(rid, l)| match l {
            Some(stored) if stored.to_lowercase() == q_lower => Some(rid),
            _ => None,
        })
        .collect();
    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() > 1 {
        anyhow::bail!(
            "label '{}' is ambiguous — {} of your rooms share that title",
            label_query,
            matches.len()
        );
    }
    let cid = matches.remove(0);
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT channel, room_key FROM conversations WHERE id = ?",
    )
    .bind(cid)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(ch, k)| (cid, ch, k)))
}

/// List all rooms a user is a member of, with their labels.
/// Used in DM mode to expose cross-room context to the speaker.
pub async fn rooms_for_user(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<(i64, Option<String>)>> {
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT c.id, c.label FROM conversations c \
         JOIN conv_members m ON m.conv_id = c.id \
         WHERE m.user_id = ? AND c.kind = 'room' \
         ORDER BY c.id ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
