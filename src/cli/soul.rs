//! `folkbot soul show / history / edit / rollback / lock / unlock`.

use anyhow::Result;
use colored::Colorize;
use sqlx::SqlitePool;

use super::SoulSub;
use crate::storage::soul::{self, Field, Op, Patch, SoulCard};
use crate::util::fmt_ts;

pub(crate) async fn run(cmd: SoulSub, pool: &SqlitePool) -> Result<()> {
    match cmd {
        SoulSub::Show => {
            let card = SoulCard::load(pool).await?;
            println!(
                "{}  {}  {}",
                "Soul card".bright_cyan().bold(),
                format!("(revision {})", card.revision).dimmed(),
                format!("updated {}", fmt_ts(card.updated_at)).dimmed()
            );
            println!("{}", "─".repeat(60).dimmed());
            print!("{}", card.format_for_prompt());
            println!("{}", "─".repeat(60).dimmed());
            if !card.locked_fields.is_empty() {
                println!(
                    "{} {}",
                    "🔒 locked:".dimmed(),
                    card.locked_fields.join(", ").dimmed()
                );
            }
            Ok(())
        }
        SoulSub::History { last } => {
            let rows = soul::revision_log(pool, last).await?;
            if rows.is_empty() {
                println!("{}", "(no revisions yet)".dimmed());
                return Ok(());
            }
            println!(
                "{:>4}  {:<19}  {:<22}  {:<8}  {:>15}  {}",
                "rev".bold(),
                "when".bold(),
                "field".bold(),
                "op".bold(),
                "Δchars".bold(),
                "reason".bold()
            );
            for r in rows {
                let delta = if r.char_delta >= 0 {
                    format!("+{}", r.char_delta).green().to_string()
                } else {
                    format!("{}", r.char_delta).red().to_string()
                };
                println!(
                    "{:>4}  {:<19}  {:<22}  {:<8}  {:>15}  {}",
                    r.revision,
                    fmt_ts(r.applied_at),
                    r.field,
                    r.op,
                    delta,
                    r.reason
                );
            }
            Ok(())
        }
        SoulSub::Edit { field, op, content, reason } => {
            let f = Field::parse(&field)
                .ok_or_else(|| anyhow::anyhow!("unknown field '{}'", field))?;
            let o = Op::parse(&op)
                .ok_or_else(|| anyhow::anyhow!("unknown op '{}' (use add|modify|remove)", op))?;
            let rev = soul::apply_patch(pool, Patch { field: f, op: o, content, reason }).await?;
            println!("{} revision {}", "✓".green().bold(), rev);
            Ok(())
        }
        SoulSub::Rollback { rev } => {
            let undone = soul::rollback(pool, rev).await?;
            println!(
                "{} rolled back {} revision(s) — now at revision {}",
                "✓".green().bold(),
                undone,
                rev
            );
            Ok(())
        }
        SoulSub::Lock { field } => {
            soul::lock_field(pool, &field).await?;
            println!("{} locked '{}'", "🔒".to_string(), field);
            Ok(())
        }
        SoulSub::Unlock { field, reason } => {
            soul::unlock_field(pool, &field, &reason).await?;
            println!("{} unlocked '{}'", "🔓".to_string(), field);
            Ok(())
        }
    }
}
