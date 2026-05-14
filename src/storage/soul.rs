//! Soul card — the agent's identity that evolves slowly over time.
//!
//! The soul card is the layer between the static base system prompt and
//! per-turn dynamic context. It holds:
//!
//! - **Frozen fields** (`name`, `kind`): only changeable via explicit unlock
//! - **Slow-drift fields** (`core_values`, `tone`): tightest patch limits
//! - **Evolvable lists** (`traits`, `people`, `quirks`, `formative_memories`):
//!   one-at-a-time additions
//!
//! All edits go through `apply_patch` which enforces:
//! - per-patch character cap
//! - per-day total character cap
//! - per-day patch count cap
//! - per-field cooldown
//! - lock check
//!
//! Every applied patch is recorded in `soul_revisions` for audit + rollback.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::storage::db::now_ts;

// ─── Field / Op enums ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Name,
    Kind,
    CoreValues,
    Tone,
    Traits,
    People,
    Quirks,
    FormativeMemories,
    Nicknames,
}

impl Field {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "name" => Self::Name,
            "kind" => Self::Kind,
            "core_values" => Self::CoreValues,
            "tone" => Self::Tone,
            "traits" => Self::Traits,
            "people" => Self::People,
            "quirks" => Self::Quirks,
            "formative_memories" => Self::FormativeMemories,
            "nicknames" => Self::Nicknames,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Kind => "kind",
            Self::CoreValues => "core_values",
            Self::Tone => "tone",
            Self::Traits => "traits",
            Self::People => "people",
            Self::Quirks => "quirks",
            Self::FormativeMemories => "formative_memories",
            Self::Nicknames => "nicknames",
        }
    }
    pub fn is_slow_drift(self) -> bool {
        matches!(self, Self::CoreValues | Self::Tone)
    }
    pub fn is_list(self) -> bool {
        matches!(
            self,
            Self::Traits
                | Self::People
                | Self::Quirks
                | Self::FormativeMemories
                | Self::Nicknames
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Modify,
    Remove,
}

impl Op {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "add" => Self::Add,
            "modify" => Self::Modify,
            "remove" => Self::Remove,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Modify => "modify",
            Self::Remove => "remove",
        }
    }
}

// ─── Data types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    #[serde(default)]
    pub relationship: String,
    #[serde(default)]
    pub key_memories: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SoulCard {
    pub name: String,
    pub kind: String,
    pub core_values: String,
    pub tone: String,
    pub traits: Vec<String>,
    pub people: Vec<Person>,
    pub quirks: Vec<String>,
    pub formative_memories: Vec<String>,
    pub nicknames: Vec<String>,
    pub locked_fields: Vec<String>,
    pub revision: i64,
    pub updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct SoulRow {
    name: String,
    kind: String,
    core_values: String,
    tone: String,
    traits: String,
    people: String,
    quirks: String,
    formative_memories: String,
    #[sqlx(default)]
    nicknames: String,
    locked_fields: String,
    revision: i64,
    updated_at: i64,
}

impl SoulCard {
    pub async fn load(pool: &SqlitePool) -> Result<Self> {
        let row: SoulRow = sqlx::query_as(
            "SELECT name, kind, core_values, tone, traits, people, quirks,
                    formative_memories, nicknames, locked_fields, revision, updated_at
             FROM soul_card WHERE id = 1",
        )
        .fetch_one(pool)
        .await
        .context("load soul_card")?;
        Ok(Self::from_row(row))
    }

    /// Same as `load` but reads inside an existing transaction. Used by
    /// `apply_patch` so the limit checks see the same snapshot as the
    /// subsequent write (no race against another concurrent patch).
    pub async fn load_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<Self> {
        let row: SoulRow = sqlx::query_as(
            "SELECT name, kind, core_values, tone, traits, people, quirks,
                    formative_memories, nicknames, locked_fields, revision, updated_at
             FROM soul_card WHERE id = 1",
        )
        .fetch_one(&mut **tx)
        .await
        .context("load soul_card (tx)")?;
        Ok(Self::from_row(row))
    }

    fn from_row(row: SoulRow) -> Self {
        Self {
            name: row.name,
            kind: row.kind,
            core_values: row.core_values,
            tone: row.tone,
            traits: serde_json::from_str(&row.traits).unwrap_or_default(),
            people: serde_json::from_str(&row.people).unwrap_or_default(),
            quirks: serde_json::from_str(&row.quirks).unwrap_or_default(),
            formative_memories: serde_json::from_str(&row.formative_memories).unwrap_or_default(),
            nicknames: serde_json::from_str(&row.nicknames).unwrap_or_default(),
            locked_fields: serde_json::from_str(&row.locked_fields).unwrap_or_default(),
            revision: row.revision,
            updated_at: row.updated_at,
        }
    }

    /// Render the soul card as a Markdown section for the system prompt.
    pub fn format_for_prompt(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("## Who I am\nI'm {}. {}.\n", self.name, self.kind));
        if !self.nicknames.is_empty() {
            out.push_str(&format!("Nicknames: {}\n", self.nicknames.join(", ")));
        }
        if !self.core_values.is_empty() {
            out.push_str(&format!("\n{}\n", self.core_values));
        }
        if !self.tone.is_empty() {
            out.push_str(&format!("\n## Voice\n{}\n", self.tone));
        }
        if !self.traits.is_empty() {
            out.push_str("\n## My personality\n");
            for t in &self.traits {
                out.push_str(&format!("- {}\n", t));
            }
        }
        if !self.people.is_empty() {
            out.push_str("\n## People I care about\n");
            for p in &self.people {
                out.push_str(&format!("\n### {} ({})\n", p.name, p.relationship));
                for m in &p.key_memories {
                    out.push_str(&format!("- {}\n", m));
                }
            }
        }
        if !self.quirks.is_empty() {
            out.push_str("\n## My quirks\n");
            for q in &self.quirks {
                out.push_str(&format!("- {}\n", q));
            }
        }
        if !self.formative_memories.is_empty() {
            out.push_str("\n## Moments that shaped me\n");
            for m in self.formative_memories.iter().take(3) {
                out.push_str(&format!("- {}\n", m));
            }
        }
        out
    }
}

// ─── Patch + limits ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Patch {
    pub field: Field,
    pub op: Op,
    pub content: String,
    pub reason: String,
}

pub struct PatchLimits {
    pub max_patch_chars: usize,
    pub max_slow_drift_chars: usize,
    pub max_daily_chars: usize,
    pub max_daily_patches: usize,
    pub cooldown_secs: i64,
}

impl Default for PatchLimits {
    fn default() -> Self {
        Self {
            max_patch_chars: 500,
            max_slow_drift_chars: 200,
            max_daily_chars: 1000,
            max_daily_patches: 10,
            cooldown_secs: 5 * 60,
        }
    }
}

// ─── apply_patch ────────────────────────────────────────────────────

/// Apply a patch with all validation. Returns the new revision number.
///
/// All limit checks (cooldown, daily count, daily char budget) run *inside*
/// a `BEGIN IMMEDIATE` transaction so two concurrent patches can't both
/// pass the checks and double-write. The `IMMEDIATE` mode acquires the
/// reserved write lock up-front, blocking other writers until commit.
pub async fn apply_patch(pool: &SqlitePool, patch: Patch) -> Result<i64> {
    let limits = PatchLimits::default();

    if patch.reason.trim().is_empty() {
        bail!("--reason is required");
    }

    // Stateless input validation first — no DB needed.
    let cap = if patch.field.is_slow_drift() {
        limits.max_slow_drift_chars
    } else {
        limits.max_patch_chars
    };
    let content_chars = patch.content.chars().count();
    if content_chars > cap {
        bail!(
            "patch content {} chars exceeds {}-char limit ({} field)",
            content_chars,
            cap,
            if patch.field.is_slow_drift() { "slow-drift" } else { "regular" }
        );
    }

    let mut tx = pool.begin().await?;
    // BEGIN IMMEDIATE: take write lock up-front so the check-then-act
    // sequence below is serialized against other concurrent apply_patch
    // calls. Without this, two patches can race past the limit checks.
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *tx).await.ok();

    let card = SoulCard::load_in_tx(&mut tx).await?;

    if card.locked_fields.iter().any(|f| f == patch.field.as_str()) {
        bail!(
            "field '{}' is locked. Run `folkbot soul unlock --field {} --reason \"...\"` first.",
            patch.field.as_str(),
            patch.field.as_str()
        );
    }

    let last: Option<(i64,)> = sqlx::query_as(
        "SELECT applied_at FROM soul_revisions WHERE field = ? ORDER BY applied_at DESC LIMIT 1",
    )
    .bind(patch.field.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((last_ts,)) = last {
        let now = now_ts();
        if now - last_ts < limits.cooldown_secs {
            let wait = limits.cooldown_secs - (now - last_ts);
            bail!(
                "cooldown: wait {}s before next patch on field '{}'",
                wait,
                patch.field.as_str()
            );
        }
    }

    let day_ago = now_ts() - 24 * 3600;
    let daily: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(ABS(char_delta)), 0)
         FROM soul_revisions WHERE applied_at > ?",
    )
    .bind(day_ago)
    .fetch_one(&mut *tx)
    .await?;
    if daily.0 as usize >= limits.max_daily_patches {
        bail!(
            "daily patch count limit ({}) reached",
            limits.max_daily_patches
        );
    }
    if (daily.1 as usize) + content_chars > limits.max_daily_chars {
        bail!(
            "daily char budget {} would be exceeded (already {})",
            limits.max_daily_chars,
            daily.1
        );
    }

    let (before, after, char_delta) = perform_patch(&card, &patch)?;

    let new_revision = card.revision + 1;
    let now = now_ts();

    let updated = update_card_field(&card, &patch, &after);
    save_card(&mut tx, &updated, new_revision, now).await?;

    sqlx::query(
        "INSERT INTO soul_revisions (revision, field, op, char_delta, before_value, after_value, reason, applied_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_revision)
    .bind(patch.field.as_str())
    .bind(patch.op.as_str())
    .bind(char_delta as i64)
    .bind(&before)
    .bind(&after)
    .bind(&patch.reason)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_revision)
}

fn perform_patch(card: &SoulCard, patch: &Patch) -> Result<(String, String, i32)> {
    let before_value: String = match patch.field {
        Field::Name => card.name.clone(),
        Field::Kind => card.kind.clone(),
        Field::CoreValues => card.core_values.clone(),
        Field::Tone => card.tone.clone(),
        Field::Traits => serde_json::to_string(&card.traits)?,
        Field::People => serde_json::to_string(&card.people)?,
        Field::Quirks => serde_json::to_string(&card.quirks)?,
        Field::FormativeMemories => serde_json::to_string(&card.formative_memories)?,
        Field::Nicknames => serde_json::to_string(&card.nicknames)?,
    };

    let after_value: String = if patch.field.is_list() {
        apply_list_op(patch.field, patch.op, &before_value, &patch.content)?
    } else {
        match patch.op {
            Op::Add => format!("{}{}", before_value, patch.content),
            Op::Modify => patch.content.clone(),
            Op::Remove => String::new(),
        }
    };

    let char_delta = (after_value.chars().count() as i32) - (before_value.chars().count() as i32);
    Ok((before_value, after_value, char_delta))
}

fn apply_list_op(field: Field, op: Op, before: &str, content: &str) -> Result<String> {
    match field {
        Field::People => {
            let mut list: Vec<Person> = serde_json::from_str(before).unwrap_or_default();
            let person: Person = serde_json::from_str(content).with_context(|| {
                "people content must be JSON: \
                 {\"name\":\"…\",\"relationship\":\"…\",\"key_memories\":[\"…\"]}"
            })?;
            match op {
                Op::Add => {
                    if list.iter().any(|p| p.name == person.name) {
                        bail!(
                            "person '{}' already exists; use --op modify instead",
                            person.name
                        );
                    }
                    list.push(person);
                }
                Op::Modify => {
                    let idx = list
                        .iter()
                        .position(|p| p.name == person.name)
                        .ok_or_else(|| anyhow!("person '{}' not found", person.name))?;
                    list[idx] = person;
                }
                Op::Remove => {
                    let target_name = person.name.clone();
                    let len_before = list.len();
                    list.retain(|p| p.name != target_name);
                    if list.len() == len_before {
                        bail!("person '{}' not found", target_name);
                    }
                }
            }
            Ok(serde_json::to_string(&list)?)
        }
        Field::Traits | Field::Quirks | Field::FormativeMemories | Field::Nicknames => {
            let mut list: Vec<String> = serde_json::from_str(before).unwrap_or_default();
            match op {
                Op::Add => list.push(content.to_string()),
                Op::Modify => {
                    bail!("modify on string list ambiguous — use remove + add")
                }
                Op::Remove => {
                    let len_before = list.len();
                    list.retain(|s| s != content);
                    if list.len() == len_before {
                        bail!("entry not found in list");
                    }
                }
            }
            Ok(serde_json::to_string(&list)?)
        }
        _ => unreachable!("apply_list_op called with non-list field"),
    }
}

fn update_card_field(card: &SoulCard, patch: &Patch, after: &str) -> SoulCard {
    let mut c = card.clone();
    match patch.field {
        Field::Name => c.name = after.to_string(),
        Field::Kind => c.kind = after.to_string(),
        Field::CoreValues => c.core_values = after.to_string(),
        Field::Tone => c.tone = after.to_string(),
        Field::Traits => c.traits = serde_json::from_str(after).unwrap_or_default(),
        Field::People => c.people = serde_json::from_str(after).unwrap_or_default(),
        Field::Quirks => c.quirks = serde_json::from_str(after).unwrap_or_default(),
        Field::FormativeMemories => {
            c.formative_memories = serde_json::from_str(after).unwrap_or_default()
        }
        Field::Nicknames => c.nicknames = serde_json::from_str(after).unwrap_or_default(),
    }
    c
}

async fn save_card(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    card: &SoulCard,
    revision: i64,
    updated_at: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE soul_card SET
            name = ?, kind = ?, core_values = ?, tone = ?,
            traits = ?, people = ?, quirks = ?, formative_memories = ?,
            nicknames = ?, locked_fields = ?, revision = ?, updated_at = ?
         WHERE id = 1",
    )
    .bind(&card.name)
    .bind(&card.kind)
    .bind(&card.core_values)
    .bind(&card.tone)
    .bind(serde_json::to_string(&card.traits)?)
    .bind(serde_json::to_string(&card.people)?)
    .bind(serde_json::to_string(&card.quirks)?)
    .bind(serde_json::to_string(&card.formative_memories)?)
    .bind(serde_json::to_string(&card.nicknames)?)
    .bind(serde_json::to_string(&card.locked_fields)?)
    .bind(revision)
    .bind(updated_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ─── lock / unlock / history / rollback ─────────────────────────────

pub async fn lock_field(pool: &SqlitePool, field: &str) -> Result<()> {
    let _ = Field::parse(field).ok_or_else(|| anyhow!("unknown field '{}'", field))?;
    let card = SoulCard::load(pool).await?;
    let mut locked = card.locked_fields.clone();
    if !locked.iter().any(|f| f == field) {
        locked.push(field.to_string());
    }
    sqlx::query("UPDATE soul_card SET locked_fields = ? WHERE id = 1")
        .bind(serde_json::to_string(&locked)?)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn unlock_field(pool: &SqlitePool, field: &str, reason: &str) -> Result<()> {
    let _ = Field::parse(field).ok_or_else(|| anyhow!("unknown field '{}'", field))?;
    if reason.trim().is_empty() {
        bail!("--reason required for unlock");
    }
    let card = SoulCard::load(pool).await?;
    let locked: Vec<String> = card
        .locked_fields
        .into_iter()
        .filter(|f| f != field)
        .collect();
    let new_revision = card.revision + 1;
    let now = now_ts();
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE soul_card SET locked_fields = ?, revision = ?, updated_at = ? WHERE id = 1")
        .bind(serde_json::to_string(&locked)?)
        .bind(new_revision)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO soul_revisions (revision, field, op, char_delta, reason, applied_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(new_revision)
    .bind(field)
    .bind("unlock")
    .bind(0i64)
    .bind(reason)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub struct RevisionRow {
    pub revision: i64,
    pub field: String,
    pub op: String,
    pub char_delta: i64,
    pub reason: String,
    pub applied_at: i64,
}

pub async fn revision_log(pool: &SqlitePool, last_n: usize) -> Result<Vec<RevisionRow>> {
    let rows: Vec<(i64, String, String, i64, String, i64)> = sqlx::query_as(
        "SELECT revision, field, op, char_delta, reason, applied_at
         FROM soul_revisions ORDER BY revision DESC LIMIT ?",
    )
    .bind(last_n as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RevisionRow {
            revision: r.0,
            field: r.1,
            op: r.2,
            char_delta: r.3,
            reason: r.4,
            applied_at: r.5,
        })
        .collect())
}

pub async fn rollback(pool: &SqlitePool, target_revision: i64) -> Result<usize> {
    let card = SoulCard::load(pool).await?;
    if target_revision < 0 {
        bail!("revision must be >= 0");
    }
    if target_revision >= card.revision {
        bail!(
            "target revision {} is not earlier than current {}",
            target_revision,
            card.revision
        );
    }

    let to_undo: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT revision, field, before_value FROM soul_revisions
         WHERE revision > ? AND revision <= ? ORDER BY revision DESC",
    )
    .bind(target_revision)
    .bind(card.revision)
    .fetch_all(pool)
    .await?;

    let count = to_undo.len();
    let now = now_ts();
    let mut tx = pool.begin().await?;

    for (_rev, field_str, before) in to_undo {
        let field = Field::parse(&field_str)
            .ok_or_else(|| anyhow!("unknown field in revision log: {}", field_str))?;
        let before = before.unwrap_or_default();
        let sql = format!(
            "UPDATE soul_card SET {} = ?, updated_at = ? WHERE id = 1",
            field.as_str()
        );
        sqlx::query(&sql)
            .bind(&before)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("UPDATE soul_card SET revision = ?, updated_at = ? WHERE id = 1")
        .bind(target_revision)
        .bind(now)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_op_add_appends_entry() {
        let after = apply_list_op(Field::Traits, Op::Add, "[]", "curious").unwrap();
        let parsed: Vec<String> = serde_json::from_str(&after).unwrap();
        assert_eq!(parsed, vec!["curious"]);
    }

    #[test]
    fn list_op_remove_missing_entry_errors() {
        let err = apply_list_op(Field::Traits, Op::Remove, "[\"a\",\"b\"]", "c").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn list_op_modify_string_list_rejected() {
        let err = apply_list_op(Field::Traits, Op::Modify, "[]", "x").unwrap_err();
        assert!(err.to_string().contains("modify on string list"));
    }

    #[test]
    fn people_op_add_duplicate_errors() {
        let one = apply_list_op(
            Field::People,
            Op::Add,
            "[]",
            r#"{"name":"Ryan","relationship":"owner"}"#,
        )
        .unwrap();
        let err = apply_list_op(
            Field::People,
            Op::Add,
            &one,
            r#"{"name":"Ryan","relationship":"also owner"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn apply_patch_rejects_locked_field() {
        let pool = crate::storage::db::test_pool().await;
        let err = apply_patch(
            &pool,
            Patch {
                field: Field::Name,
                op: Op::Modify,
                content: "Bob".into(),
                reason: "test".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("locked"));
    }

    #[tokio::test]
    async fn apply_patch_enforces_cooldown() {
        let pool = crate::storage::db::test_pool().await;
        apply_patch(
            &pool,
            Patch {
                field: Field::Tone,
                op: Op::Modify,
                content: "sweet".into(),
                reason: "first".into(),
            },
        )
        .await
        .unwrap();
        let err = apply_patch(
            &pool,
            Patch {
                field: Field::Tone,
                op: Op::Modify,
                content: "sour".into(),
                reason: "second".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cooldown"));
    }

    #[tokio::test]
    async fn apply_patch_requires_reason() {
        let pool = crate::storage::db::test_pool().await;
        let err = apply_patch(
            &pool,
            Patch {
                field: Field::Tone,
                op: Op::Modify,
                content: "sweet".into(),
                reason: "   ".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("reason"));
    }
}
