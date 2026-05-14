//! `folkbot chat` — interactive REPL with streaming output and slash commands.

use anyhow::{Context, Result};
use colored::Colorize;
use rustyline::error::ReadlineError;
use sqlx::SqlitePool;
use std::io::Write;

use super::bootstrap::{bootstrap, Bootstrap};
use crate::agent::TurnEvent;
use crate::slash;
use crate::storage::summaries;
use crate::storage::users::{self, Principal, UserRole};

const HISTORY_FILE: &str = ".folkbot_history";

pub(crate) async fn run(config_path: &str, pool: &SqlitePool) -> Result<()> {
    let Bootstrap {
        cfg,
        summarizer_llm,
        core,
        bg_handle,
        watcher_handle,
        mcp_summary,
    } = bootstrap(config_path, pool).await?;
    let summarizer_label = cfg
        .agent
        .summarizer
        .as_ref()
        .map(|s| format!("{}/{}", s.provider, s.model))
        .unwrap_or_else(|| "(same as main)".to_string());
    let registry_size = core.registry.names().len();

    let principal = Principal::cli_local();
    let mut current_user = users::lookup_by_principal(pool, &principal).await?;

    println!(
        "{} v{} ready  ({} / {})  summarizer: {}",
        "Folkbot".bright_cyan().bold(),
        env!("CARGO_PKG_VERSION"),
        cfg.llm.provider.dimmed(),
        cfg.llm.model.dimmed(),
        summarizer_label.dimmed(),
    );
    let bg_label = if cfg.agent.compressor.interval_seconds > 0 {
        format!("every {}s", cfg.agent.compressor.interval_seconds)
    } else {
        "off".into()
    };
    let tools_summary = format!(
        "tools: {}{}  · bg compressor: {}",
        registry_size,
        if mcp_summary.is_empty() { "".to_string() } else { format!(" (mcp: {})", mcp_summary.join(", ")) },
        bg_label,
    );
    println!("  {}", tools_summary.dimmed());
    match &current_user {
        Some(u) => println!("  {}", format!("you: {}", u.name).dimmed()),
        None => println!(
            "  {}",
            format!("you: (unknown — principal {})", principal.label()).dimmed()
        ),
    }
    println!(
        "{}",
        "  enter to send · Ctrl+D to quit (auto-summarize)".dimmed()
    );

    let mut rl = rustyline::DefaultEditor::new().context("init readline")?;
    let _ = rl.load_history(HISTORY_FILE);

    'repl: loop {
        // Rebuild prompt prefix each turn so identification / role changes show.
        let prompt_str = match &current_user {
            Some(u) => {
                let badge = match u.role {
                    UserRole::Owner => "👑 ",
                    UserRole::ViceOwner => "★ ",
                    UserRole::Regular => "",
                };
                format!(
                    "{}{} {} ",
                    badge,
                    u.name.bright_cyan(),
                    "›".bright_blue().bold()
                )
            }
            None => format!("{} ", "›".bright_blue().bold()),
        };

        let line = match rl.readline(&prompt_str) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => {
                println!("{}", "(Ctrl+C — type Ctrl+D to quit)".dimmed());
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!("{}", "bye 🐶".dimmed());
                break 'repl;
            }
            Err(e) => anyhow::bail!("readline: {}", e),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(trimmed);

        // Slash commands intercept before the LLM round-trip. `/` alone
        // pops up a fuzzy menu; `/cmd args` parses + dispatches as if it
        // were `folkbot cmd args` from the shell.
        if trimmed.starts_with('/') {
            match slash::handle(trimmed, pool, config_path, current_user.as_ref()).await {
                Ok(slash::SlashOutcome::Done) => {}
                Ok(slash::SlashOutcome::Exit) => {
                    println!("{}", "bye 🐶".dimmed());
                    break 'repl;
                }
                Ok(slash::SlashOutcome::ResetHistory) => {
                    println!(
                        "{}",
                        "(in-memory window cleared; persistent log untouched)".dimmed()
                    );
                }
                Ok(slash::SlashOutcome::Logout) => {
                    let was = current_user
                        .as_ref()
                        .map(|u| u.name.clone())
                        .unwrap_or_else(|| "(none)".into());
                    let removed = users::unlink_principal(pool, &principal).await?;
                    current_user = None;
                    if removed {
                        println!(
                            "  {} logged out {} — next turn starts as anonymous",
                            "·".dimmed(),
                            was.bright_white().bold()
                        );
                    } else {
                        println!("  {} not currently logged in as any user", "·".dimmed());
                    }
                }
                Err(e) => eprintln!("{} {:#}", "✗".red().bold(), e),
            }
            continue;
        }

        let mut printed_label = false;
        // Captured by `on_event` if `user_identify` returns
        // `challenge_required`. Drained after `run_turn` so the password
        // prompt happens once Folkbot's text has finished rendering — and so
        // the password capture is OUTSIDE the LLM loop entirely.
        let mut pending_challenge: Option<(String, i64, String)> = None;
        let mut on_event = |evt: &TurnEvent| match evt {
            TurnEvent::Text(t) => {
                if !printed_label {
                    print!("{} ", "Folkbot".bright_green().bold());
                    printed_label = true;
                }
                print!("{}", t);
                std::io::stdout().flush().ok();
            }
            TurnEvent::ToolStart { name, args } => {
                if !printed_label {
                    print!("{} ", "Folkbot".bright_green().bold());
                    printed_label = true;
                }
                println!(
                    "\n{} {}{}",
                    "→".dimmed(),
                    name.dimmed(),
                    format!("({})", args).dimmed()
                );
            }
            TurnEvent::ToolResult { name, result } => {
                println!("{} {}", "←".dimmed(), result.to_string().dimmed());
                if name == "user_identify" {
                    if result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        if let Some(uname) = result.get("name").and_then(|v| v.as_str()) {
                            let uid = result.get("user_id").and_then(|v| v.as_i64()).unwrap_or(0);
                            println!(
                                "  {} linked as {} {}",
                                "✓".bright_green().bold(),
                                uname.bright_white().bold(),
                                format!("(id={}, role=regular)", uid).dimmed()
                            );
                        }
                    } else if result
                        .get("challenge_required")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        if let (Some(uname), Some(uid), Some(urole)) = (
                            result.get("name").and_then(|v| v.as_str()),
                            result.get("user_id").and_then(|v| v.as_i64()),
                            result.get("role").and_then(|v| v.as_str()),
                        ) {
                            pending_challenge =
                                Some((uname.to_string(), uid, urole.to_string()));
                        }
                    }
                }
            }
        };

        match core
            .run_turn(
                &principal,
                crate::agent::TurnInput::text_only(trimmed),
                None,
                &mut on_event,
            )
            .await
        {
            Ok(reply) => {
                if reply.is_empty() && !printed_label {
                    println!("{}", "(empty reply)".dimmed());
                } else {
                    println!();
                }
                current_user = users::lookup_by_principal(pool, &principal).await?;
            }
            Err(e) => {
                eprintln!("\n{} {:#}", "✗".red().bold(), e);
            }
        }

        // Password challenge runs AFTER the LLM turn. The password is
        // captured via rpassword (terminal echo off) and never touches
        // the LLM, the messages table, or rustyline history.
        if let Some((name, user_id, role)) = pending_challenge {
            let badge = match role.as_str() {
                "owner" => "👑 owner".bright_yellow().to_string(),
                "vice_owner" => "★ vice_owner".bright_cyan().to_string(),
                _ => "bound (regular)".dimmed().to_string(),
            };
            println!(
                "  {} {} is {} — enter verification password (read straight from terminal, never sent to LLM or written to DB)",
                "🔐".to_string(),
                name.bright_white().bold(),
                badge
            );
            match rpassword::prompt_password("Password: ") {
                Ok(pw) if pw.is_empty() => {
                    println!("  {} cancelled (empty password)", "·".dimmed());
                }
                Ok(pw) => {
                    let hash = users::get_password_hash(pool, user_id).await?;
                    match hash {
                        None => {
                            println!(
                                "  {} this account has no password set (shouldn't reach here, possible race)",
                                "✗".red().bold()
                            );
                        }
                        Some(h) => match users::verify_password(&h, &pw) {
                            Ok(true) => {
                                users::link_principal(pool, user_id, &principal).await?;
                                current_user =
                                    users::lookup_by_principal(pool, &principal).await?;
                                println!(
                                    "  {} verified as {} {}",
                                    "✓".bright_green().bold(),
                                    name.bright_white().bold(),
                                    format!("(id={}, role={})", user_id, role).dimmed()
                                );
                                tracing::info!(
                                    "password-verified link: {} → user '{}' (id={}, role={})",
                                    principal.label(),
                                    name,
                                    user_id,
                                    role
                                );
                            }
                            Ok(false) => {
                                println!("  {} wrong password", "✗".red().bold());
                            }
                            Err(e) => {
                                println!("  {} verification failed: {:#}", "✗".red().bold(), e);
                            }
                        },
                    }
                }
                Err(_) => {
                    println!("  {} cancelled", "·".dimmed());
                }
            }
        }
    }

    let _ = rl.save_history(HISTORY_FILE);
    if let Some(h) = bg_handle {
        h.abort();
    }
    watcher_handle.abort();

    // Session-end: rolling vibe + yesterday's daily + cascade roll-up.
    if let Some(u) = &current_user {
        if let Err(e) = summaries::refresh_user_vibe(pool, summarizer_llm.as_ref(), u.id, &u.name).await.map(|_| ()) {
            eprintln!("{} session summary failed: {:#}", "!".yellow().bold(), e);
        }
        match summaries::ensure_yesterday_daily(pool, summarizer_llm.as_ref(), u.id, &u.name).await
        {
            Ok(true) => println!(
                "{} {}",
                "·".dimmed(),
                format!("daily summary written for yesterday ({})", u.name).dimmed()
            ),
            Ok(false) => {}
            Err(e) => eprintln!("{} daily compressor failed: {:#}", "!".yellow().bold(), e),
        }
        match summaries::cascade_rollup(pool, summarizer_llm.as_ref(), u.id, &u.name).await {
            Ok(rolled) if !rolled.is_empty() => {
                let names: Vec<&str> = rolled.iter().map(|p| p.as_str()).collect();
                println!(
                    "{} {}",
                    "·".dimmed(),
                    format!("rolled up: {}", names.join(" → ")).dimmed()
                );
            }
            Ok(_) => {}
            Err(e) => eprintln!("{} cascade roll-up failed: {:#}", "!".yellow().bold(), e),
        }
    }

    Ok(())
}
