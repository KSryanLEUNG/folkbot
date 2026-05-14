//! User identity layer.
//!
//! A `Principal` is "who's on the other end of a channel" — for CLI it's
//! `os_user@hostname`, for Telegram it's the telegram user id. A `User` is
//! Folkbot's internal identity, named (e.g. "Ryan"). One user can be linked to
//! multiple principals (CLI + Telegram + Discord).
//!
//! Mapping: `(channel, principal_id) → user_id` via `user_principals`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::storage::db::now_ts;

/// Three-level permission model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Regular,
    ViceOwner,
    Owner,
}

impl UserRole {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "owner" => Self::Owner,
            "vice_owner" | "vice-owner" | "viceowner" | "vice" => Self::ViceOwner,
            "regular" | "user" | "none" => Self::Regular,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::ViceOwner => "vice_owner",
            Self::Owner => "owner",
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            Self::Regular => 0,
            Self::ViceOwner => 1,
            Self::Owner => 2,
        }
    }
    /// True if `self` has at least the privileges of `other`.
    pub fn at_least(self, other: UserRole) -> bool {
        self.rank() >= other.rank()
    }
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub channel: String,      // "cli" | "telegram" | …
    pub principal_id: String, // channel-specific id
}

impl Principal {
    pub fn cli_local() -> Self {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| whoami::username());
        let host = hostname::get()
            .ok()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_else(|| "localhost".into());
        Self {
            channel: "cli".into(),
            principal_id: format!("{}@{}", user, host),
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.channel, self.principal_id)
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub last_summary: Option<String>,
    pub last_summary_ts: Option<i64>,
    pub role: UserRole,
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    name: String,
    display_name: Option<String>,
    last_summary: Option<String>,
    last_summary_ts: Option<i64>,
    #[sqlx(default)]
    role: String,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            display_name: r.display_name,
            last_summary: r.last_summary,
            last_summary_ts: r.last_summary_ts,
            role: UserRole::parse(&r.role).unwrap_or(UserRole::Regular),
        }
    }
}

/// Look up the user mapped to the current principal, if any.
pub async fn lookup_by_principal(
    pool: &SqlitePool,
    principal: &Principal,
) -> Result<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT u.id, u.name, u.display_name, u.last_summary, u.last_summary_ts, u.role
         FROM users u
         JOIN user_principals p ON p.user_id = u.id
         WHERE p.channel = ? AND p.principal_id = ?",
    )
    .bind(&principal.channel)
    .bind(&principal.principal_id)
    .fetch_optional(pool)
    .await
    .context("lookup user by principal")?;
    Ok(row.map(Into::into))
}

/// Look up a user by name (case-insensitive).
pub async fn lookup_by_name(pool: &SqlitePool, name: &str) -> Result<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT id, name, display_name, last_summary, last_summary_ts, role
         FROM users WHERE name = ? COLLATE NOCASE",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn lookup_by_id(pool: &SqlitePool, user_id: i64) -> Result<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT id, name, display_name, last_summary, last_summary_ts, role
         FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn list_all(pool: &SqlitePool) -> Result<Vec<User>> {
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT id, name, display_name, last_summary, last_summary_ts, role
         FROM users ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Bulk lookup. Returns a map of unique user_ids to Users. Missing ids
/// are simply absent. Used by the personal-timeline renderer to resolve
/// many speaker_ids in one go.
///
/// SQLite doesn't support binding `Vec<i64>` to a single `?` placeholder,
/// so we build the placeholder list manually. Caller-side IDs are i64 so
/// no SQL-injection surface — we're stringifying integers.
pub async fn lookup_many_by_ids(
    pool: &SqlitePool,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, User>> {
    let unique: std::collections::HashSet<i64> = ids.iter().copied().filter(|&i| i > 0).collect();
    if unique.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders: Vec<&str> = (0..unique.len()).map(|_| "?").collect();
    let sql = format!(
        "SELECT id, name, display_name, last_summary, last_summary_ts, role
         FROM users WHERE id IN ({})",
        placeholders.join(","),
    );
    let mut q = sqlx::query_as::<_, UserRow>(&sql);
    for uid in &unique {
        q = q.bind(uid);
    }
    let rows: Vec<UserRow> = q.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let u: User = r.into();
            (u.id, u)
        })
        .collect())
}

pub async fn set_role(pool: &SqlitePool, user_id: i64, role: UserRole) -> Result<()> {
    sqlx::query("UPDATE users SET role = ? WHERE id = ?")
        .bind(role.as_str())
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Find or create user by name (case-insensitive), then link the principal.
/// Returns the user_id.
pub async fn upsert_and_link(
    pool: &SqlitePool,
    name: &str,
    principal: &Principal,
) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let now = now_ts();

    let existing: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM users WHERE name = ? COLLATE NOCASE")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
    let user_id = match existing {
        Some((id,)) => id,
        None => {
            let res = sqlx::query("INSERT INTO users (name, created_at) VALUES (?, ?)")
                .bind(name)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            res.last_insert_rowid()
        }
    };

    sqlx::query(
        "INSERT INTO user_principals (channel, principal_id, user_id, linked_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(channel, principal_id) DO UPDATE SET user_id = excluded.user_id",
    )
    .bind(&principal.channel)
    .bind(&principal.principal_id)
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(user_id)
}

pub async fn save_summary(pool: &SqlitePool, user_id: i64, summary: &str) -> Result<()> {
    sqlx::query(
        "UPDATE users SET last_summary = ?, last_summary_ts = ? WHERE id = ?",
    )
    .bind(summary)
    .bind(now_ts())
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Password challenge (CLI-side privileged-identity verification) ──

/// OWASP 2023 recommended argon2id parameters: m=19 MiB, t=3, p=1.
/// Stronger than the crate default (which is m=19 MiB, t=2, p=1) on the
/// time-cost dimension, ~50% more work per attempt.
fn argon2_owasp() -> argon2::Argon2<'static> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(19_456, 3, 1, None).expect("valid argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash and store a password for a user via argon2id. Used by
/// `folkbot user set-password`. Each call generates a fresh salt — calling
/// twice on the same plaintext produces different hashes.
pub async fn set_password(pool: &SqlitePool, user_id: i64, password: &str) -> Result<()> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use rand_core::OsRng;
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2_owasp()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash: {}", e))?
        .to_string();
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&hash)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn clear_password(pool: &SqlitePool, user_id: i64) -> Result<()> {
    sqlx::query("UPDATE users SET password_hash = NULL WHERE id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_password_hash(pool: &SqlitePool, user_id: i64) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT password_hash FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(h,)| h))
}

/// Constant-time-ish password verification via argon2. Returns Ok(false)
/// for a wrong password (so callers can distinguish "wrong" from "errored").
///
/// Verification reads the cost params from the stored hash itself (PHC
/// string format), so this still validates against legacy hashes if the
/// cost defaults change. New hashes use `argon2_owasp()` params.
pub fn verify_password(hash: &str, password: &str) -> Result<bool> {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let parsed =
        PasswordHash::new(hash).map_err(|e| anyhow::anyhow!("parse argon2 hash: {}", e))?;
    Ok(argon2_owasp()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// True if `user_id` already has at least one principal mapped on any
/// channel. Used by `IdentifyUser` to gate name re-claim: a name that's
/// already linked to *some* principal can't be silently re-claimed by a
/// fresh principal — the legitimate owner has to either log out (drops
/// the mapping, allowing re-claim) or use the password challenge.
pub async fn has_any_principal(pool: &SqlitePool, user_id: i64) -> Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM user_principals WHERE user_id = ? LIMIT 1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

/// Bind an existing user_id to a principal. Used after CLI password
/// verification — the user already exists, we're just adding a new
/// principal mapping. Different from `upsert_and_link` which would
/// create the user too.
pub async fn link_principal(
    pool: &SqlitePool,
    user_id: i64,
    principal: &Principal,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO user_principals (channel, principal_id, user_id, linked_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(channel, principal_id) DO UPDATE SET user_id = excluded.user_id",
    )
    .bind(&principal.channel)
    .bind(&principal.principal_id)
    .bind(user_id)
    .bind(now_ts())
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop the principal mapping. Returns true if a row was deleted.
/// Used by `/logout` to revert this CLI session to anonymous so a
/// different user can identify (and pass any password challenge).
pub async fn unlink_principal(
    pool: &SqlitePool,
    principal: &Principal,
) -> Result<bool> {
    let res = sqlx::query(
        "DELETE FROM user_principals WHERE channel = ? AND principal_id = ?",
    )
    .bind(&principal.channel)
    .bind(&principal.principal_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}
