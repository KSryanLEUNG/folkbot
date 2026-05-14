//! SQLite pool + schema migrations.
//!
//! Tables created here (idempotent migrations, all `CREATE … IF NOT EXISTS`):
//! - `soul_card` — single-row agent character card (id=1)
//! - `soul_revisions` — append-only audit log of every soul edit
//! - `messages` — per-(user, conversation) chat log
//! - `users` + `user_principals` — identity layer
//! - `facts` — top tier of the context pyramid
//! - `summaries` — middle tier (day → week → month → quarter)
//! - `conversations` + `conv_members` — v1.2 shared-room support

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

pub async fn init_pool(db_path: impl AsRef<Path>) -> Result<SqlitePool> {
    let path = db_path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)
        .context("parse sqlite url")?
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .context("connect to sqlite")?;

    // WAL mode allows the background compressor to read while the chat REPL
    // is mid-write (and vice versa) without blocking on the same single
    // writer lock. Critical once we run a periodic background task.
    sqlx::query("PRAGMA journal_mode = WAL").execute(&pool).await?;
    sqlx::query("PRAGMA synchronous = NORMAL").execute(&pool).await?;

    migrate(&pool).await?;
    Ok(pool)
}

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS soul_card (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            core_values TEXT NOT NULL DEFAULT '',
            tone TEXT NOT NULL DEFAULT '',
            traits TEXT NOT NULL DEFAULT '[]',
            people TEXT NOT NULL DEFAULT '[]',
            quirks TEXT NOT NULL DEFAULT '[]',
            formative_memories TEXT NOT NULL DEFAULT '[]',
            nicknames TEXT NOT NULL DEFAULT '[]',
            locked_fields TEXT NOT NULL DEFAULT '["name","kind"]',
            revision INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS soul_revisions (
            revision INTEGER PRIMARY KEY,
            field TEXT NOT NULL,
            op TEXT NOT NULL,
            char_delta INTEGER NOT NULL,
            before_value TEXT,
            after_value TEXT,
            reason TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_revisions_field_ts ON soul_revisions(field, applied_at)")
        .execute(pool)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
            content TEXT NOT NULL,
            conversation_id INTEGER NOT NULL DEFAULT 0,
            user_id INTEGER NOT NULL DEFAULT 0
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_conv_ts ON messages(conversation_id, ts)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_user_ts ON messages(user_id, ts)")
        .execute(pool)
        .await?;
    // v1.3: composite index speeding up `load_personal_timeline`'s
    // `WHERE (conversation_id=0 AND user_id=?) OR conversation_id IN (...)
    //  ORDER BY ts DESC` on a large messages table. The combined
    // (conv_id, user_id, ts) covers both branches of the OR.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_messages_conv_user_ts \
         ON messages(conversation_id, user_id, ts)",
    )
    .execute(pool)
    .await?;

    // Pre-v0.4 messages table didn't have user_id. Backfill column for
    // very old DBs; new DBs already have it from the CREATE TABLE above.
    let msg_cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as("PRAGMA table_info(messages)")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    if !msg_cols.iter().any(|c| c.1 == "user_id") {
        sqlx::query("ALTER TABLE messages ADD COLUMN user_id INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await?;
    }

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE,
            display_name TEXT,
            last_summary TEXT,
            last_summary_ts INTEGER,
            created_at INTEGER NOT NULL,
            role TEXT NOT NULL DEFAULT 'regular'
                CHECK (role IN ('owner','vice_owner','regular')),
            password_hash TEXT
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS user_principals (
            channel TEXT NOT NULL,
            principal_id TEXT NOT NULL,
            user_id INTEGER NOT NULL,
            linked_at INTEGER NOT NULL,
            PRIMARY KEY (channel, principal_id),
            FOREIGN KEY (user_id) REFERENCES users(id)
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_principals_user ON user_principals(user_id)",
    )
    .execute(pool)
    .await?;

    // Idempotent column migrations for older DBs. New DBs include `role`
    // and `password_hash` directly via the CREATE TABLE below — these
    // ALTERs only fire on pre-v1.0 / pre-v1.1 DBs that pre-date them.
    //
    // Note: the historical `is_owner` boolean column is intentionally NOT
    // re-added here. It was deprecated by `role` in v1.0; v1.3 dropped the
    // column from this codebase entirely. Old DBs that still have it work
    // fine (column just sits unused).
    let user_cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as("PRAGMA table_info(users)")
            .fetch_all(pool)
            .await
            .unwrap_or_default();

    if !user_cols.iter().any(|c| c.1 == "role") {
        sqlx::query(
            "ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'regular' \
             CHECK (role IN ('owner','vice_owner','regular'))",
        )
        .execute(pool)
        .await?;
        // Carry over any legacy is_owner=1 → role='owner'.
        if user_cols.iter().any(|c| c.1 == "is_owner") {
            sqlx::query("UPDATE users SET role = 'owner' WHERE is_owner = 1")
                .execute(pool)
                .await?;
        }
    }

    // Argon2 password hash for CLI-side challenge auth. NULL = no password
    // set → privileged-name claim from CLI is rejected.
    if !user_cols.iter().any(|c| c.1 == "password_hash") {
        sqlx::query("ALTER TABLE users ADD COLUMN password_hash TEXT")
            .execute(pool)
            .await?;
    }

    // Idempotent migration: add soul_card.nicknames if missing.
    let soul_cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as("PRAGMA table_info(soul_card)")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    if !soul_cols.iter().any(|c| c.1 == "nicknames") {
        sqlx::query("ALTER TABLE soul_card ADD COLUMN nicknames TEXT NOT NULL DEFAULT '[]'")
            .execute(pool)
            .await?;
    }

    // ─── v1.2: shared rooms (group chat semantics) ──────────────
    // `conversations` tracks distinct conversation spaces. DM messages keep
    // their legacy hard-coded conversation_id=1; rooms get auto-incremented
    // ids (>=2). UNIQUE(channel, room_key) ensures one room per Telegram
    // group chat (channel='telegram', room_key=stringified chat_id).
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS conversations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('dm','room')),
            channel TEXT,
            room_key TEXT,
            label TEXT,
            created_at INTEGER NOT NULL,
            UNIQUE (channel, room_key)
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS conv_members (
            conv_id INTEGER NOT NULL,
            user_id INTEGER NOT NULL,
            joined_at INTEGER NOT NULL,
            PRIMARY KEY (conv_id, user_id)
        )"#,
    )
    .execute(pool)
    .await?;

    // ─── v0.5: facts + summaries ────────────────────────────────
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS facts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL DEFAULT 0,
            importance TEXT NOT NULL CHECK (importance IN ('H','M','L')),
            content TEXT NOT NULL,
            tags TEXT NOT NULL DEFAULT '[]',
            source_msg_id INTEGER,
            created_at INTEGER NOT NULL
        )"#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_facts_user_imp ON facts(user_id, importance)")
        .execute(pool)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS summaries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL DEFAULT 0,
            period TEXT NOT NULL CHECK (period IN ('day','week','month','quarter')),
            start_ts INTEGER NOT NULL,
            end_ts INTEGER NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE (user_id, period, start_ts, end_ts)
        )"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_summaries_user_period_end ON summaries(user_id, period, end_ts DESC)",
    )
    .execute(pool)
    .await?;

    // Seed a default soul card on first run.
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM soul_card")
        .fetch_one(pool)
        .await?;
    if count.0 == 0 {
        let now = now_ts();
        sqlx::query(
            r#"INSERT INTO soul_card (id, name, kind, tone, updated_at)
               VALUES (1, 'Folkbot', 'family-oriented AI assistant', 'warm, concise, not bureaucratic', ?)"#,
        )
        .bind(now)
        .execute(pool)
        .await?;
    }

    Ok(())
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) async fn test_pool() -> SqlitePool {
    // Single-connection in-memory pool so all queries within a test see the
    // same DB state. WAL is unnecessary (no concurrent writers in tests).
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory sqlite");
    migrate(&pool).await.expect("test migrate");
    pool
}
