//! `folkbot summaries list / roll-up`.

use anyhow::Result;
use colored::Colorize;
use sqlx::SqlitePool;

use super::SummariesSub;
use crate::config::Config;
use crate::storage::summaries::{self, Period};
use crate::storage::users::{self, Principal};
use crate::util::fmt_ts;

pub(crate) async fn run(cmd: SummariesSub, config_path: &str, pool: &SqlitePool) -> Result<()> {
    match cmd {
        SummariesSub::List { user, period, last } => {
            let period_enum = Period::parse(&period)
                .ok_or_else(|| anyhow::anyhow!("unknown period '{}'", period))?;
            let user_id = resolve_user_id(pool, user.as_deref()).await?;
            let list = summaries::list_recent(pool, user_id, period_enum, last).await?;
            if list.is_empty() {
                println!("{}", "(no summaries)".dimmed());
                return Ok(());
            }
            for s in list {
                println!(
                    "{} {}  {}~{}",
                    s.period.as_str().bold(),
                    format!("(u{}, id={})", s.user_id, s.id).dimmed(),
                    fmt_ts(s.start_ts),
                    fmt_ts(s.end_ts)
                );
                println!("{}", s.content);
                println!("{}", "─".repeat(60).dimmed());
            }
            Ok(())
        }
        SummariesSub::RollUp { period, user } => {
            let target = Period::parse(&period)
                .ok_or_else(|| anyhow::anyhow!("unknown period '{}'", period))?;
            let user_id = resolve_user_id(pool, user.as_deref()).await?;
            let cfg = Config::load(config_path)?;
            let (_main, summarizer_llm) = cfg.build_llms()?;
            let user_obj = users::list_all(pool)
                .await?
                .into_iter()
                .find(|u| u.id == user_id)
                .ok_or_else(|| anyhow::anyhow!("user_id {} not found", user_id))?;
            match summaries::roll_up(pool, summarizer_llm.as_ref(), user_id, &user_obj.name, target)
                .await?
            {
                true => println!("{} rolled up to {}", "✓".green().bold(), target.as_str()),
                false => println!("{} already exists", "·".dimmed()),
            }
            Ok(())
        }
    }
}

async fn resolve_user_id(pool: &SqlitePool, name: Option<&str>) -> Result<i64> {
    match name {
        Some(n) => users::lookup_by_name(pool, n)
            .await?
            .map(|u| u.id)
            .ok_or_else(|| anyhow::anyhow!("no user named '{}'", n)),
        None => {
            let principal = Principal::cli_local();
            users::lookup_by_principal(pool, &principal)
                .await?
                .map(|u| u.id)
                .ok_or_else(|| anyhow::anyhow!("not identified — start a chat first"))
        }
    }
}
