//! `folkbot user list / link / set-role / set-password / clear-password`.

use anyhow::{Context, Result};
use colored::Colorize;
use sqlx::SqlitePool;

use super::UserSub;
use crate::storage::users::{self, Principal, UserRole};
use crate::util::fmt_ts;

pub(crate) async fn run(cmd: UserSub, pool: &SqlitePool) -> Result<()> {
    match cmd {
        UserSub::List => {
            let users = users::list_all(pool).await?;
            if users.is_empty() {
                println!("{}", "(no users yet — start a chat to be identified)".dimmed());
                return Ok(());
            }
            println!(
                "{:>4}  {:<10}  {:<16}  {:<19}  {}",
                "id".bold(),
                "role".bold(),
                "name".bold(),
                "summary updated".bold(),
                "summary".bold()
            );
            for u in users {
                let when = u
                    .last_summary_ts
                    .map(fmt_ts)
                    .unwrap_or_else(|| "—".to_string());
                let sum = u
                    .last_summary
                    .as_deref()
                    .unwrap_or("(none)")
                    .chars()
                    .take(60)
                    .collect::<String>();
                let role_label = match u.role {
                    UserRole::Owner => "👑 owner".bright_yellow().to_string(),
                    UserRole::ViceOwner => "vice".cyan().to_string(),
                    UserRole::Regular => "regular".dimmed().to_string(),
                };
                println!(
                    "{:>4}  {:<10}  {:<16}  {:<19}  {}",
                    u.id, role_label, u.name, when, sum
                );
            }
            Ok(())
        }
        UserSub::Link { name, channel, principal_id } => {
            let user = users::lookup_by_name(pool, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no user named '{}'", name))?;
            let principal = Principal {
                channel: channel.clone(),
                principal_id: principal_id.clone(),
            };
            users::upsert_and_link(pool, &user.name, &principal).await?;
            println!(
                "{} linked {} → user '{}' (id {})",
                "✓".green().bold(),
                principal.label(),
                user.name,
                user.id
            );
            Ok(())
        }
        UserSub::SetRole { name, role } => {
            let parsed = UserRole::parse(&role).ok_or_else(|| {
                anyhow::anyhow!("invalid role '{}' — use owner | vice_owner | regular", role)
            })?;
            let user = users::lookup_by_name(pool, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no user named '{}'", name))?;
            users::set_role(pool, user.id, parsed).await?;
            println!(
                "{} {} → role={}",
                "✓".green().bold(),
                user.name,
                parsed.as_str()
            );
            Ok(())
        }
        UserSub::SetPassword { name } => {
            let user = users::lookup_by_name(pool, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no user named '{}'", name))?;
            let pw = rpassword::prompt_password(format!("New password for {}: ", user.name))
                .context("read password")?;
            if pw.is_empty() {
                anyhow::bail!("empty password rejected");
            }
            let confirm = rpassword::prompt_password("Confirm: ").context("read confirm")?;
            if pw != confirm {
                anyhow::bail!("passwords do not match");
            }
            users::set_password(pool, user.id, &pw).await?;
            println!(
                "{} password set for {} {}",
                "✓".green().bold(),
                user.name,
                format!("(role={})", user.role.as_str()).dimmed()
            );
            Ok(())
        }
        UserSub::ClearPassword { name } => {
            let user = users::lookup_by_name(pool, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no user named '{}'", name))?;
            users::clear_password(pool, user.id).await?;
            println!("{} password cleared for {}", "✓".green().bold(), user.name);
            Ok(())
        }
    }
}
