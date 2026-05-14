//! Telegram channel adapter with streaming-edit replies.
//!
//! Pipeline per inbound message:
//!   1. allowlist check
//!   2. classify the message (text / photo / voice / sticker / document)
//!   3. send placeholder ("…") so we have a `message_id` to edit
//!   4. spawn editor task that drains streaming text from a tokio mpsc
//!      and edits the placeholder periodically (rate-limit safe)
//!   5. forward to `AgentCore::run_turn` with a sync sink that pushes
//!      text deltas onto the channel
//!   6. on stream end, editor flushes the final state
//!
//! Why mpsc + spawn: `run_turn` takes a sync `&mut FnMut(&TurnEvent)` sink.
//! Telegram I/O is async. The channel decouples them — sink writes
//! synchronously into the queue, editor task does Telegram edits with
//! awaiting + rate limiting.

mod addressing;
mod inbound;
mod outbound;
mod streaming;

pub use outbound::TelegramOutbound;

use anyhow::Result;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::ChatAction;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::{AgentCore, TurnEvent};
use crate::config::TelegramConfig;
use crate::llm::Role;
use crate::storage::users::{self, Principal};

use addressing::{is_addressed_to_bot, strip_leading_mention};
use inbound::classify_inbound;
use streaming::{chunk_for_telegram, stream_to_message, SAFE_EDIT_LIMIT};

pub async fn spawn(core: Arc<AgentCore>, cfg: TelegramConfig) -> Result<JoinHandle<()>> {
    let token = cfg.resolve_token()?;
    // Keep the raw token alive for v1.4 file downloads (https://api.telegram.org/file/bot<TOKEN>/...).
    let token_arc: Arc<String> = Arc::new(token.clone());
    let bot = Bot::new(token);

    let me = bot
        .get_me()
        .await
        .map_err(|e| anyhow::anyhow!("Telegram getMe failed: {}", e))?;
    let username = me.username().to_string();
    println!("Telegram bot @{} ready (allowed: {})", username, cfg.allowed_users.len());

    let allowed: Arc<Vec<String>> = Arc::new(cfg.allowed_users);
    let bot_username: Arc<String> = Arc::new(username);
    let core_for_handler = core.clone();

    let handle = tokio::spawn(async move {
        teloxide::repl(bot, move |bot: Bot, msg: Message| {
            let core = core_for_handler.clone();
            let allowed = allowed.clone();
            let bot_username = bot_username.clone();
            let token = token_arc.clone();
            async move {
                if let Err(e) =
                    handle_message(core, allowed, bot_username, token, bot, msg).await
                {
                    tracing::warn!("telegram handler error: {:#}", e);
                }
                respond(())
            }
        })
        .await;
    });
    Ok(handle)
}

async fn handle_message(
    core: Arc<AgentCore>,
    allowed: Arc<Vec<String>>,
    bot_username: Arc<String>,
    token: Arc<String>,
    bot: Bot,
    msg: Message,
) -> Result<()> {
    let from = match msg.from.as_ref() {
        Some(u) => u,
        None => return Ok(()),
    };
    let from_id = from.id.0.to_string();

    if !allowed.iter().any(|a| a == &from_id) {
        tracing::info!("telegram: rejecting unknown user {}", from_id);
        return Ok(());
    }

    // v1.4: classify the inbound message — it might be plain text, or it
    // might be a photo / voice / sticker / document. Returns None when the
    // message has nothing we can route (system events, empty text).
    let intake = match classify_inbound(&core, &bot, &token, &msg).await {
        Ok(Some(i)) => i,
        Ok(None) => return Ok(()),
        Err(e) => {
            tracing::warn!("classify_inbound failed: {:#}", e);
            return Ok(());
        }
    };
    let raw_text = intake.raw_text.clone();

    if let Some(stripped) = raw_text.strip_prefix('/') {
        let cmd = stripped.split_whitespace().next().unwrap_or("");
        if matches!(cmd, "start" | "help") {
            let _ = bot
                .send_message(
                    msg.chat.id,
                    "Hi, I'm Folkbot. Just send me a message — there are no special commands.",
                )
                .await;
            return Ok(());
        }
    }

    let principal = Principal {
        channel: "telegram".into(),
        principal_id: from_id.clone(),
    };
    let chat_id = msg.chat.id;

    // v1.2: group chats become a shared room conversation. Private DM keeps
    // the legacy per-user (conv_id=None) path.
    let conv_id: Option<i64> = if msg.chat.is_group() || msg.chat.is_supergroup() {
        let room_key = chat_id.0.to_string();
        let label = msg.chat.title().map(|s| s.to_string());
        match crate::storage::conversations::lookup_or_create_room(
            &core.pool,
            "telegram",
            &room_key,
            label.as_deref(),
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!("room lookup failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    // v1.2 mode B: in a group, only run the LLM when the message is
    // addressed to the bot. Non-addressed messages are silently logged
    // into the room timeline so Folkbot has the surrounding context next time
    // someone calls him in this room. v1.4: when the inbound is media,
    // we log the marker (e.g. `[image] caption`) — bytes are not stored.
    let mut input = intake.input;
    if let Some(cid) = conv_id {
        let addressed = is_addressed_to_bot(&core.pool, &msg, &bot_username).await;
        if !addressed {
            if let Ok(Some(user)) =
                users::lookup_by_principal(&core.pool, &principal).await
            {
                let _ = crate::storage::conversations::add_member(&core.pool, cid, user.id).await;
                let _ = crate::storage::messages::append_room(
                    &core.pool,
                    cid,
                    user.id,
                    Role::User,
                    &input.text,
                )
                .await;
            }
            return Ok(());
        }
        // Addressed: strip the leading @mention from the persisted text so
        // Folkbot's context isn't polluted with its own handle as the first
        // token. Only meaningful when the input is plain text — for media
        // turns, the @mention is part of the caption and we leave it alone.
        if input.media.is_empty() {
            input.text = strip_leading_mention(&input.text, &bot_username);
        }
        if input.text.trim().is_empty() && input.media.is_empty() {
            // Bare "@folkbot" with no other content — treat as a ping; respond
            // with a short hello rather than dispatch an empty turn to LLM.
            let _ = bot.send_message(chat_id, "here 🐶").await;
            return Ok(());
        }
    }

    let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

    let placeholder = match bot.send_message(chat_id, "…").await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("placeholder send failed: {}", e);
            return Ok(());
        }
    };

    // mpsc: agent-side sync sink → editor-side async Telegram I/O.
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let editor_bot = bot.clone();
    let editor: JoinHandle<()> = tokio::spawn(async move {
        stream_to_message(editor_bot, chat_id, placeholder.id, rx).await;
    });

    let mut sink = move |evt: &TurnEvent| {
        if let TurnEvent::Text(t) = evt {
            let _ = tx.send(t.clone());
        }
    };

    let result = core.run_turn(&principal, input, conv_id, &mut sink).await;
    drop(sink); // drops tx → editor sees rx close → final flush

    let _ = editor.await;

    match result {
        Ok(reply) => {
            let count = reply.chars().count();
            if count > SAFE_EDIT_LIMIT {
                let remainder: String = reply.chars().skip(SAFE_EDIT_LIMIT).collect();
                for chunk in chunk_for_telegram(&remainder) {
                    if let Err(e) = bot.send_message(chat_id, chunk).await {
                        tracing::warn!("overflow chunk send failed: {}", e);
                    }
                }
            }
            if count == 0 {
                let _ = bot
                    .edit_message_text(chat_id, placeholder.id, "(nothing to say on my end)")
                    .await;
            }
        }
        Err(e) => {
            tracing::error!("agent turn failed: {:#}", e);
            let _ = bot
                .edit_message_text(chat_id, placeholder.id, "(something went wrong, give me a sec...)")
                .await;
        }
    }
    Ok(())
}
