//! `folkbot history show / count / clear`.

use anyhow::Result;
use colored::Colorize;
use sqlx::SqlitePool;

use super::HistorySub;
use crate::llm::Role;
use crate::storage::messages;
use crate::storage::users::{self, Principal};
use crate::util::fmt_ts;

pub(crate) async fn run(cmd: HistorySub, pool: &SqlitePool) -> Result<()> {
    let principal = Principal::cli_local();
    let user = users::lookup_by_principal(pool, &principal).await?;

    match cmd {
        HistorySub::Count => {
            let n = match &user {
                Some(u) => messages::count(pool, Some(u.id)).await?,
                None => 0,
            };
            println!("{} messages in your log", n);
            Ok(())
        }
        HistorySub::Show { last } => {
            let user = user.ok_or_else(|| {
                anyhow::anyhow!(
                    "this principal ({}) is not yet identified — start a chat session first",
                    principal.label()
                )
            })?;
            let msgs = messages::load_last_n(pool, user.id, last).await?;
            if msgs.is_empty() {
                println!("{}", "(no messages yet)".dimmed());
                return Ok(());
            }
            for (m, ts) in msgs {
                let (label, color_role) = match m.role {
                    Role::User => (user.name.as_str(), "›".bright_blue().bold().to_string()),
                    Role::Assistant => ("Folkbot", "·".bright_green().bold().to_string()),
                    _ => continue,
                };
                let content = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                println!(
                    "{}  {} {}  {}",
                    fmt_ts(ts).dimmed(),
                    color_role,
                    label.dimmed(),
                    content
                );
            }
            Ok(())
        }
        HistorySub::Clear { confirm } => {
            if !confirm {
                anyhow::bail!("destructive — pass `--confirm` to actually wipe");
            }
            let n = match &user {
                Some(u) => messages::clear(pool, Some(u.id)).await?,
                None => 0,
            };
            println!("{} deleted {} messages from your log", "✓".green().bold(), n);
            Ok(())
        }
    }
}
