//! `folkbot facts list / add / remove`.

use anyhow::Result;
use colored::Colorize;
use sqlx::SqlitePool;

use super::FactsSub;
use crate::storage::facts::{self, Fact, Importance};
use crate::storage::users::{self, Principal};
use crate::util::fmt_ts;

pub(crate) async fn run(cmd: FactsSub, pool: &SqlitePool) -> Result<()> {
    match cmd {
        FactsSub::List { user, importance } => {
            let user_id = match user {
                Some(name) => match users::lookup_by_name(pool, &name).await? {
                    Some(u) => u.id,
                    None => anyhow::bail!("no user named '{}'", name),
                },
                None => {
                    let principal = Principal::cli_local();
                    users::lookup_by_principal(pool, &principal)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("not identified — start a chat first"))?
                        .id
                }
            };
            let imp_filter = importance
                .as_deref()
                .and_then(Importance::parse);
            let facts = facts::list_for_user(pool, user_id, true).await?;
            let filtered: Vec<&Fact> = facts
                .iter()
                .filter(|f| imp_filter.map(|i| f.importance == i).unwrap_or(true))
                .collect();
            if filtered.is_empty() {
                println!("{}", "(no matching facts)".dimmed());
                return Ok(());
            }
            println!(
                "{:>4}  {:<3}  {:>6}  {:<19}  {}",
                "id".bold(),
                "imp".bold(),
                "owner".bold(),
                "when".bold(),
                "content".bold()
            );
            for f in filtered {
                let owner = if f.user_id == 0 {
                    "shared".to_string()
                } else {
                    format!("u{}", f.user_id)
                };
                let imp_str = match f.importance {
                    Importance::H => "H".red().bold().to_string(),
                    Importance::M => "M".yellow().to_string(),
                    Importance::L => "L".dimmed().to_string(),
                };
                println!(
                    "{:>4}  {:<3}  {:>6}  {:<19}  {}",
                    f.id,
                    imp_str,
                    owner,
                    fmt_ts(f.created_at),
                    f.content
                );
            }
            Ok(())
        }
        FactsSub::Add { content, importance, scope } => {
            let imp = Importance::parse(&importance)
                .ok_or_else(|| anyhow::anyhow!("importance must be H, M, or L"))?;
            let user_id = match scope.as_str() {
                "shared" => 0,
                "self" => {
                    let principal = Principal::cli_local();
                    users::lookup_by_principal(pool, &principal)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("not identified — start a chat first"))?
                        .id
                }
                other => anyhow::bail!("scope must be 'self' or 'shared', got '{}'", other),
            };
            let id = facts::add(pool, user_id, imp, &content, &[]).await?;
            println!("{} fact id={}", "✓".green().bold(), id);
            Ok(())
        }
        FactsSub::Remove { id } => {
            if facts::remove(pool, id).await? {
                println!("{} removed fact {}", "✓".green().bold(), id);
            } else {
                anyhow::bail!("no fact with id {}", id);
            }
            Ok(())
        }
    }
}
