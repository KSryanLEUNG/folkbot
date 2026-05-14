//! CLI subcommand definitions + dispatcher.
//!
//! Every `folkbot <subcommand>` lives in its own file under `cli/`. This module
//! defines the clap structs (so `--help` works centrally) and routes
//! parsed commands to the right handler.

mod bootstrap;
mod chat;
pub(crate) mod facts;
pub(crate) mod history;
pub(crate) mod prompt;
mod serve;
pub(crate) mod soul;
pub(crate) mod summaries;
pub(crate) mod user;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sqlx::SqlitePool;

#[derive(Parser)]
#[command(name = "folkbot", version, about = "Always-on family AI agent — shared identity, persistent memory, Telegram + CLI")]
pub(crate) struct Cli {
    #[arg(short, long, default_value = "folkbot.toml")]
    pub(crate) config: String,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Start the interactive chat REPL (default).
    Chat,
    /// Run as a long-lived daemon serving channels (Telegram, etc.).
    Serve,
    /// Manage the soul card.
    Soul {
        #[command(subcommand)]
        cmd: SoulSub,
    },
    /// Inspect the persisted message log.
    History {
        #[command(subcommand)]
        cmd: HistorySub,
    },
    /// User directory.
    User {
        #[command(subcommand)]
        cmd: UserSub,
    },
    /// Top-tier facts (durable, importance-tagged knowledge).
    Facts {
        #[command(subcommand)]
        cmd: FactsSub,
    },
    /// Mid-tier summaries (daily / weekly / monthly / quarterly).
    Summaries {
        #[command(subcommand)]
        cmd: SummariesSub,
    },
    /// Show the composed system prompt that would be sent to the LLM for
    /// a given user. Useful for debugging persona / role / cross-user
    /// gating behaviour without burning tokens.
    Prompt {
        /// User name. Defaults to the current CLI principal's user, or
        /// "unidentified" if no principal is mapped.
        #[arg(long)]
        user: Option<String>,
        /// Conversation id to render in room mode (shows the prompt as it
        /// would look for a turn happening in that room). Omit for DM mode.
        #[arg(long = "conv-id")]
        conv_id: Option<i64>,
    },
}

#[derive(Subcommand)]
pub(crate) enum FactsSub {
    /// List facts (default: current user's + shared).
    List {
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        importance: Option<String>,
    },
    /// Manually add a fact (without going through the agent).
    Add {
        content: String,
        #[arg(long)]
        importance: String,
        /// "self" (current principal's user) or "shared". Default "self".
        #[arg(long, default_value = "self")]
        scope: String,
    },
    /// Delete a fact by id.
    Remove { id: i64 },
}

#[derive(Subcommand)]
pub(crate) enum SummariesSub {
    /// List recent summaries (default: current user's, daily).
    List {
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value = "day")]
        period: String,
        #[arg(long, default_value_t = 10)]
        last: usize,
    },
    /// Manually trigger an aggregation roll-up.
    RollUp {
        period: String,
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum SoulSub {
    Show,
    History {
        #[arg(long, default_value_t = 20)]
        last: usize,
    },
    Edit {
        field: String,
        op: String,
        content: String,
        #[arg(long)]
        reason: String,
    },
    Rollback {
        rev: i64,
    },
    Lock {
        #[arg(long)]
        field: String,
    },
    Unlock {
        #[arg(long)]
        field: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum HistorySub {
    /// Show the last N messages for the current principal's user.
    Show {
        #[arg(long, default_value_t = 20)]
        last: usize,
    },
    Count,
    Clear {
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum UserSub {
    /// List all known users.
    List,
    /// Link a channel principal to an existing user.
    /// Useful when the same person uses both CLI and Telegram and you
    /// want them to share one identity (and one memory).
    Link {
        /// Existing user name (case-insensitive).
        name: String,
        /// Channel: "telegram", "cli", etc.
        #[arg(long)]
        channel: String,
        /// Channel-specific principal ID (e.g., telegram numeric user id).
        #[arg(long)]
        principal_id: String,
    },
    /// Set a user's role: `owner` | `vice_owner` | `regular`.
    /// - `owner`     → can `soul_patch`, can read raw cross-user transcripts
    /// - `vice_owner`→ sees other users' summaries (no patch, no raw)
    /// - `regular`   → only own world; no cross-user visibility
    SetRole {
        name: String,
        /// One of: owner, vice_owner, regular
        role: String,
    },
    /// Set a CLI verification password for a privileged user. Required if
    /// you want to self-claim that identity from a fresh CLI principal
    /// without admin override. Prompts twice (masked); never accepted as
    /// a flag to avoid shell-history leakage.
    SetPassword { name: String },
    /// Remove the password from a user — claims from new CLI principals
    /// will be rejected again (anti-impersonation reverts to admin-only
    /// `user link` override).
    ClearPassword { name: String },
}

/// Parse the command line, apply timezone, dispatch to the right handler.
pub(crate) async fn run(pool: &SqlitePool) -> Result<()> {
    let cli = Cli::parse();

    // Apply [agent].timezone_offset_secs before any subcommand runs — they
    // all use fmt_ts/fmt_date. If config can't load, we fall through to the
    // util.rs default (HK +8h, matching v1.2 behaviour).
    if let Ok(cfg) = crate::config::Config::load(&cli.config) {
        crate::util::set_tz_offset(cfg.agent.tz_offset());
    }

    match cli.command {
        Some(Command::Soul { cmd }) => soul::run(cmd, pool).await,
        Some(Command::History { cmd }) => history::run(cmd, pool).await,
        Some(Command::User { cmd }) => user::run(cmd, pool).await,
        Some(Command::Facts { cmd }) => facts::run(cmd, pool).await,
        Some(Command::Summaries { cmd }) => summaries::run(cmd, &cli.config, pool).await,
        Some(Command::Serve) => serve::run(&cli.config, pool).await,
        Some(Command::Prompt { user, conv_id }) => {
            prompt::run(user.as_deref(), conv_id, &cli.config, pool).await
        }
        Some(Command::Chat) | None => chat::run(&cli.config, pool).await,
    }
}
