//! Slash commands inside the chat REPL.
//!
//! Two entry points:
//!   1. User types `/` (alone) and hits Enter → fuzzy popup of all commands;
//!      pick with ↑↓ + filter as you type, Enter to confirm. If the chosen
//!      command takes args, a follow-up input box is shown.
//!   2. User types `/<cmd> <args>` directly → parsed via the same clap CLI
//!      and dispatched. Same syntax as `folkbot <cmd> <args>` from the shell.
//!
//! Slash dispatch piggybacks on the existing `Cli` type, so the CLI surface
//! and slash surface stay in lockstep — adding a new subcommand to `Cli`
//! makes it instantly available as `/<subcommand>`.

use anyhow::Result;
use colored::Colorize;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{FuzzySelect, Input};
use sqlx::SqlitePool;

use crate::storage::users::{User, UserRole};

/// What the chat REPL should do after a slash command runs.
#[derive(Debug)]
pub enum SlashOutcome {
    /// Command ran (or no-op). REPL continues normally.
    Done,
    /// User asked to quit (`/exit`, `/quit`, `/q`).
    Exit,
    /// User asked to clear in-memory window (`/reset`).
    ResetHistory,
    /// User asked to drop the current principal-to-user link
    /// (`/logout` or `/switch`). Main REPL deletes the mapping
    /// and reverts to anonymous so the next turn can identify
    /// as someone else (with password challenge if privileged).
    Logout,
}

/// One row in the slash menu.
struct Cmd {
    /// Command text — passed to clap as if typed verbatim.
    name: &'static str,
    /// One-liner shown in the popup.
    desc: &'static str,
    /// If true, the command takes no args and runs immediately.
    /// If false, after picking, we prompt for args via an Input box.
    no_args: bool,
    /// If true, hidden from non-owner pickers and rejected at dispatch
    /// when current REPL user isn't owner. Plugs the
    /// `regular-walks-up-and-self-promotes` hole.
    owner_only: bool,
}

const COMMANDS: &[Cmd] = &[
    Cmd { name: "help",                desc: "List slash commands",                                 no_args: true,  owner_only: false },
    Cmd { name: "whoami",              desc: "Show your principal + linked user",                   no_args: true,  owner_only: false },
    Cmd { name: "exit",                desc: "Quit chat REPL (same as Ctrl+D)",                     no_args: true,  owner_only: false },
    Cmd { name: "logout",              desc: "Unlink current principal — go anonymous to switch user", no_args: true, owner_only: false },
    Cmd { name: "reset",               desc: "Clear in-memory history (DB untouched)",              no_args: true,  owner_only: false },
    Cmd { name: "prompt",              desc: "Show the composed system prompt for current user",    no_args: true,  owner_only: false },
    Cmd { name: "soul show",           desc: "Print current soul card",                             no_args: true,  owner_only: false },
    Cmd { name: "soul history",        desc: "Recent soul revisions",                               no_args: true,  owner_only: false },
    Cmd { name: "soul edit",           desc: "Patch a soul field (cooldown / locks apply)",         no_args: false, owner_only: true  },
    Cmd { name: "soul rollback",       desc: "Roll back to a revision",                             no_args: false, owner_only: true  },
    Cmd { name: "soul lock",           desc: "Lock a soul field",                                   no_args: false, owner_only: true  },
    Cmd { name: "soul unlock",         desc: "Unlock a soul field",                                 no_args: false, owner_only: true  },
    Cmd { name: "facts list",          desc: "List your facts (and shared)",                        no_args: true,  owner_only: false },
    Cmd { name: "facts add",           desc: "Add a fact for yourself or shared",                   no_args: false, owner_only: false },
    Cmd { name: "facts remove",        desc: "Delete a fact by id",                                 no_args: false, owner_only: false },
    Cmd { name: "user list",           desc: "List all known users + roles",                        no_args: true,  owner_only: false },
    Cmd { name: "user link",           desc: "Bind a channel principal to an existing user",        no_args: false, owner_only: true  },
    Cmd { name: "user set-role",       desc: "Set role: owner | vice_owner | regular",              no_args: false, owner_only: true  },
    Cmd { name: "user set-password",   desc: "Set a user's CLI verification password (masked)",     no_args: false, owner_only: true  },
    Cmd { name: "user clear-password", desc: "Remove a user's CLI verification password",           no_args: false, owner_only: true  },
    Cmd { name: "history show",        desc: "Show your persisted messages",                        no_args: true,  owner_only: false },
    Cmd { name: "history count",       desc: "Count of your persisted messages",                    no_args: true,  owner_only: false },
    Cmd { name: "history clear",       desc: "Wipe your persisted messages (--confirm required)",   no_args: false, owner_only: false },
    Cmd { name: "summaries list",      desc: "Recent summaries (default daily)",                    no_args: true,  owner_only: false },
    Cmd { name: "summaries roll-up",   desc: "Manually trigger week/month/quarter roll-up",         no_args: false, owner_only: true  },
];

fn is_owner(user: Option<&User>) -> bool {
    matches!(user.map(|u| u.role), Some(UserRole::Owner))
}

/// Aliases that resolve before the COMMANDS lookup (so `/switch` reuses
/// `/logout` and `/q` reuses `/exit`).
fn canonicalize(input: &str) -> &str {
    match input {
        "switch" | "su" => "logout",
        "quit" | "q" => "exit",
        "?" => "help",
        other => other,
    }
}

pub async fn handle(
    raw: &str,
    pool: &SqlitePool,
    config_path: &str,
    current_user: Option<&User>,
) -> Result<SlashOutcome> {
    let rest = raw.trim_start_matches('/').trim();
    let owner_now = is_owner(current_user);

    // Resolve the command text (either via popup or direct input).
    let chosen: String = if rest.is_empty() {
        // Hide owner-only entries when the current REPL user is not owner —
        // keeps the menu from advertising privilege-escalation paths.
        let visible: Vec<&Cmd> = COMMANDS
            .iter()
            .filter(|c| !c.owner_only || owner_now)
            .collect();
        let labels: Vec<String> = visible
            .iter()
            .map(|c| format!("{:<22} — {}", c.name, c.desc))
            .collect();
        let pick = FuzzySelect::with_theme(&ColorfulTheme::default())
            .with_prompt("/")
            .items(&labels)
            .default(0)
            .interact_opt()?;
        let Some(idx) = pick else {
            return Ok(SlashOutcome::Done); // user pressed Esc
        };
        let cmd = visible[idx];
        if cmd.no_args {
            cmd.name.to_string()
        } else {
            let args: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt(format!("/{} (args)", cmd.name))
                .allow_empty(true)
                .interact_text()?;
            if args.trim().is_empty() {
                cmd.name.to_string()
            } else {
                format!("{} {}", cmd.name, args.trim())
            }
        }
    } else {
        rest.to_string()
    };

    let trimmed_raw = chosen.trim();
    // Resolve aliases (`/switch` → `/logout`, `/q` → `/exit`, …) before
    // matching, so REPL-level handlers and the dispatch path agree.
    let trimmed = canonicalize(trimmed_raw);

    // Owner-only gate at dispatch: someone may have typed a gated command
    // directly (`/user set-role …`) without going through the menu. Reject
    // here too. Note: still permitted from shell `folkbot user set-role …`,
    // since that requires actual OS-level access to this binary.
    if !owner_now {
        let gated = COMMANDS.iter().any(|c| c.owner_only && trimmed.starts_with(c.name));
        if gated {
            println!(
                "{} {}",
                "✗".red().bold(),
                "this command requires owner permission (run from shell or upgrade your role first)".red()
            );
            return Ok(SlashOutcome::Done);
        }
    }

    // Special slash commands handled at REPL level (not via clap).
    match trimmed {
        "help" => {
            print_help(owner_now);
            return Ok(SlashOutcome::Done);
        }
        "whoami" => {
            let principal = crate::storage::users::Principal::cli_local();
            println!(
                "  {} {}",
                "principal:".dimmed(),
                principal.label().bright_white()
            );
            match current_user {
                Some(u) => {
                    let role_label = match u.role {
                        UserRole::Owner => "👑 owner".bright_yellow().to_string(),
                        UserRole::ViceOwner => "★ vice_owner".bright_cyan().to_string(),
                        UserRole::Regular => "regular".dimmed().to_string(),
                    };
                    println!(
                        "  {} {} {}",
                        "user:".dimmed(),
                        u.name.bright_white().bold(),
                        format!("(id={}, role={})", u.id, role_label).dimmed()
                    );
                }
                None => println!(
                    "  {} {}",
                    "user:".dimmed(),
                    "(anonymous — say your name to identify)".dimmed()
                ),
            }
            return Ok(SlashOutcome::Done);
        }
        "exit" => return Ok(SlashOutcome::Exit),
        "reset" => return Ok(SlashOutcome::ResetHistory),
        "logout" => return Ok(SlashOutcome::Logout),
        _ => {}
    }

    // Everything else: dispatch via the same Cli that the binary uses.
    let args = split_args(trimmed);
    let _ = trimmed_raw; // silence unused warning when no aliasing path hit
    if args.is_empty() {
        return Ok(SlashOutcome::Done);
    }
    let mut full = vec!["folkbot".to_string()];
    full.extend(args);

    use crate::cli::{Cli, Command};
    use clap::Parser;
    let cli = match Cli::try_parse_from(&full) {
        Ok(c) => c,
        Err(e) => {
            // clap formats nicely; print to stderr and continue REPL.
            let _ = e.print();
            return Ok(SlashOutcome::Done);
        }
    };

    match cli.command {
        Some(Command::Soul { cmd }) => crate::cli::soul::run(cmd, pool).await?,
        Some(Command::History { cmd }) => crate::cli::history::run(cmd, pool).await?,
        Some(Command::User { cmd }) => crate::cli::user::run(cmd, pool).await?,
        Some(Command::Facts { cmd }) => crate::cli::facts::run(cmd, pool).await?,
        Some(Command::Summaries { cmd }) => {
            crate::cli::summaries::run(cmd, config_path, pool).await?
        }
        Some(Command::Prompt { user, conv_id }) => {
            crate::cli::prompt::run(user.as_deref(), conv_id, config_path, pool).await?
        }
        Some(Command::Chat) => println!(
            "{}",
            "(already in chat — Ctrl+D or /exit to quit)".dimmed()
        ),
        Some(Command::Serve) => println!(
            "{}",
            "(serve is daemon-only; can't run from the REPL)".dimmed()
        ),
        None => {}
    }
    Ok(SlashOutcome::Done)
}

fn print_help(owner_now: bool) {
    println!("{}", "Slash commands".bright_cyan().bold());
    println!("{}", "─".repeat(60).dimmed());
    println!(
        "{}",
        "Type `/` alone for a popup picker, or `/<cmd> [args]` directly.".dimmed()
    );
    println!();
    for c in COMMANDS {
        if c.owner_only && !owner_now {
            continue;
        }
        let badge = if c.owner_only { " 👑" } else { "" };
        println!(
            "  {:<22}  {}{}",
            format!("/{}", c.name).bright_blue(),
            c.desc,
            badge.bright_yellow()
        );
    }
    if !owner_now {
        println!(
            "\n  {} {}",
            "·".dimmed(),
            "(owner-only commands hidden — login as owner via password to unlock)".dimmed()
        );
    }
}

/// Tiny shell-style argument splitter. Handles double-quoted strings so
/// commands like `/facts add "long content" --importance H` work as
/// expected. Doesn't handle escapes inside quotes — for now, complex
/// content can be entered via the wizard input box.
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            ' ' | '\t' if !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split_args;

    #[test]
    fn unquoted_args_split_on_whitespace() {
        let out = split_args("facts add hi --importance H");
        assert_eq!(out, vec!["facts", "add", "hi", "--importance", "H"]);
    }

    #[test]
    fn quoted_string_kept_as_one_arg() {
        let out = split_args("facts add \"long content with spaces\" --importance H");
        assert_eq!(
            out,
            vec![
                "facts",
                "add",
                "long content with spaces",
                "--importance",
                "H"
            ]
        );
    }

    #[test]
    fn empty_input_yields_no_args() {
        assert!(split_args("").is_empty());
        assert!(split_args("   ").is_empty());
    }
}

