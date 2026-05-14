//! Top tier of the context pyramid.
//!
//! `facts` are durable, importance-tagged statements the agent has learned
//! about a user (or `user_id = 0` for shared/household). They get pulled
//! into the system prompt every turn, subject to the token budget.
//!
//! - `[H]` always survives the budget cut, never auto-pruned
//! - `[M]` survives in second-priority slot
//! - `[L]` survives only when budget allows; first to drop

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::storage::db::now_ts;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Importance {
    H,
    M,
    L,
}

impl Importance {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "H" | "HIGH" => Self::H,
            "M" | "MEDIUM" | "MED" => Self::M,
            "L" | "LOW" => Self::L,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::H => "H",
            Self::M => "M",
            Self::L => "L",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub id: i64,
    pub user_id: i64,
    pub importance: Importance,
    pub content: String,
    /// Tags loaded from DB; reserved for tag-based retrieval. Not displayed.
    #[allow(dead_code)]
    pub tags: Vec<String>,
    pub created_at: i64,
}

#[derive(sqlx::FromRow)]
struct FactRow {
    id: i64,
    user_id: i64,
    importance: String,
    content: String,
    tags: String,
    created_at: i64,
}

impl FactRow {
    fn into_fact(self) -> Fact {
        Fact {
            id: self.id,
            user_id: self.user_id,
            importance: Importance::parse(&self.importance).unwrap_or(Importance::M),
            content: self.content,
            tags: serde_json::from_str(&self.tags).unwrap_or_default(),
            created_at: self.created_at,
        }
    }
}

pub async fn add(
    pool: &SqlitePool,
    user_id: i64,
    importance: Importance,
    content: &str,
    tags: &[String],
) -> Result<i64> {
    let content = content.trim();
    if content.is_empty() {
        bail!("fact content cannot be empty");
    }
    // Dedup: same (user_id, content) returns the existing id rather than
    // inserting a near-duplicate row. The LLM tends to call fact_remember
    // multiple times on the same statement; without this we'd accumulate
    // copies that all look the same in the prompt.
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM facts WHERE user_id = ? AND content = ? LIMIT 1",
    )
    .bind(user_id)
    .bind(content)
    .fetch_optional(pool)
    .await?;
    if let Some((id,)) = existing {
        return Ok(id);
    }
    let res = sqlx::query(
        "INSERT INTO facts (user_id, importance, content, tags, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(importance.as_str())
    .bind(content)
    .bind(serde_json::to_string(tags)?)
    .bind(now_ts())
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn remove(pool: &SqlitePool, id: i64) -> Result<bool> {
    let res = sqlx::query("DELETE FROM facts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Load facts for a user (and shared facts when `include_shared`).
/// Importance order H → M → L, then most recent first within each tier.
pub async fn list_for_user(
    pool: &SqlitePool,
    user_id: i64,
    include_shared: bool,
) -> Result<Vec<Fact>> {
    let rows: Vec<FactRow> = if include_shared {
        sqlx::query_as(
            "SELECT id, user_id, importance, content, tags, created_at FROM facts
             WHERE user_id = ? OR user_id = 0
             ORDER BY CASE importance WHEN 'H' THEN 0 WHEN 'M' THEN 1 ELSE 2 END,
                      created_at DESC",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
            "SELECT id, user_id, importance, content, tags, created_at FROM facts
             WHERE user_id = ?
             ORDER BY CASE importance WHEN 'H' THEN 0 WHEN 'M' THEN 1 ELSE 2 END,
                      created_at DESC",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?
    };
    Ok(rows.into_iter().map(|r| r.into_fact()).collect())
}

