//! `send_message` — owner/vice_owner proactive DM or room broadcast.

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::SendRateLimit;
use crate::llm::{Role, ToolSchema};
use crate::storage::messages;
use crate::storage::users::{self, UserRole};
use crate::tool::{Tool, ToolContext};

/// `send_message` lets the agent proactively deliver a message to another
/// user without waiting for them to message first. Owner / vice_owner only.
///
/// Shares its rate limit with `send_file` via an injected `SendRateLimit`.
pub struct SendMessage {
    rate: std::sync::Arc<SendRateLimit>,
}

impl SendMessage {
    pub fn new(rate: std::sync::Arc<SendRateLimit>) -> Self {
        Self { rate }
    }
}

#[derive(Deserialize)]
struct SendMessageArgs {
    /// Target user name (case-insensitive). Mutually exclusive with
    /// `target_room` — exactly one must be provided.
    #[serde(default)]
    target: Option<String>,
    /// Target room label (Telegram group title). Mutually exclusive with
    /// `target`. The asker MUST be a member of the room — non-member rooms
    /// are not visible.
    #[serde(default)]
    target_room: Option<String>,
    /// Message BODY only — just the thing to say (e.g. "mom is at work").
    /// The tool auto-prefixes the asker's name. To a user it becomes
    /// "{asker} wants me to tell you: {content}"; to a room "{asker} wants to tell you all: {content}".
    /// DO NOT write the attribution yourself — that caused mis-attribution
    /// bugs in v1.2 (LLM put the wrong name in).
    content: String,
    /// Channel to deliver via — must match the channel column in
    /// `user_principals` / `conversations`. Default: "telegram".
    #[serde(default)]
    channel: Option<String>,
    /// Override the auto-prefix asker name. Defaults to YOUR own name (the
    /// caller). Only set this if the actual originator is someone other
    /// than you (rare).
    #[serde(default)]
    from_name: Option<String>,
}

#[async_trait]
impl Tool for SendMessage {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "send_message".into(),
            description:
                "Proactively send a message to another USER (DM) or to a ROOM (group chat). \
                 Allowed for OWNER and VICE_OWNER — regular users can't trigger this. \
                 Provide EITHER `target` (user name) OR `target_room` (room label), not both. \
                 For rooms, the asker MUST be a member. \
                 IMPORTANT: `content` is just the BODY — the tool auto-prefixes attribution: \
                 to a user → \"{asker} wants me to tell you: …\"; to a room → \"{asker} wants to tell you all: …\". \
                 Do NOT write attribution in `content`. Use this only when there's a real \
                 reason to interrupt — not for chitchat."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "User name to DM (case-insensitive). Mutually exclusive with target_room."
                    },
                    "target_room": {
                        "type": "string",
                        "description": "Room label (e.g. Telegram group title) to broadcast to. Asker must be a member."
                    },
                    "content": {
                        "type": "string",
                        "description": "Message BODY only. Don't write the \"{name} wants…\" prefix — the tool adds that."
                    },
                    "channel": {
                        "type": "string",
                        "description": "Channel name; defaults to 'telegram'.",
                        "default": "telegram"
                    },
                    "from_name": {
                        "type": "string",
                        "description": "Override prefix asker name. Defaults to your own name. Only set if the originator really is someone other than you."
                    }
                },
                "required": ["content"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let parsed: SendMessageArgs =
            serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {}", e))?;

        // LLMs often pass empty strings for unused optional params instead
        // of omitting them. Normalize "" / whitespace == not provided so
        // the mutex check below doesn't reject a legitimate one-of-two call.
        let target = parsed
            .target
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let target_room = parsed
            .target_room
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        match (target, target_room) {
            (None, None) => bail!("must provide either `target` (user) or `target_room` (room)"),
            (Some(_), Some(_)) => bail!(
                "specify only one of `target` and `target_room`, not both"
            ),
            _ => {}
        }

        let uid = ctx.user_id.ok_or_else(|| anyhow!("not identified"))?;
        let me = users::lookup_by_id(&ctx.pool, uid)
            .await?
            .ok_or_else(|| anyhow!("user_id {} not found", uid))?;
        if !me.role.at_least(UserRole::ViceOwner) {
            bail!(
                "regular users can't trigger proactive messages. \
                 You are {} ({}). Only owner / vice_owner can use send_message.",
                me.name,
                me.role.as_str()
            );
        }

        self.rate.check_and_record(me.id, &me.name).await?;

        let channel_name = parsed.channel.clone().unwrap_or_else(|| "telegram".into());
        let channel = ctx
            .outbound
            .get(&channel_name)
            .ok_or_else(|| {
                anyhow!(
                    "channel '{}' not configured (this binary mode may not have outbound for it)",
                    channel_name
                )
            })?
            .clone();

        let asker_name = parsed.from_name.as_deref().unwrap_or(&me.name);

        // ─── Branch 1: DM to a user ─────────────────────────────────
        if let Some(target_name) = target {
            let target = users::lookup_by_name(&ctx.pool, target_name)
                .await?
                .ok_or_else(|| anyhow!("no user named '{}'", target_name))?;
            if target.id == me.id {
                bail!("you're trying to message yourself — just reply directly");
            }

            let row: Option<(String,)> = sqlx::query_as(
                "SELECT principal_id FROM user_principals WHERE user_id = ? AND channel = ?",
            )
            .bind(target.id)
            .bind(&channel_name)
            .fetch_optional(&ctx.pool)
            .await?;
            let principal_id = row
                .map(|r| r.0)
                .ok_or_else(|| anyhow!("{} has no {} principal", target.name, channel_name))?;

            let outbound_text = format!("{} wants me to tell you: {}", asker_name, parsed.content);
            channel.send_to_principal(&principal_id, &outbound_text).await?;

            let logged = format!("[Folkbot proactive — entrusted by {}] {}", asker_name, outbound_text);
            let _ = messages::append(&ctx.pool, target.id, Role::Assistant, &logged).await;

            tracing::info!(
                "send_message: {} (id={}, asker={}) → DM {} (id={}, via {}): {} chars",
                me.name,
                me.id,
                asker_name,
                target.name,
                target.id,
                channel_name,
                parsed.content.chars().count()
            );

            return Ok(json!({
                "ok": true,
                "kind": "dm",
                "delivered_to": target.name,
                "via": channel_name,
                "principal_id": principal_id,
                "sent_text": outbound_text,
            }));
        }

        // ─── Branch 2: broadcast to a room ──────────────────────────
        let room_label = target_room.unwrap();
        let resolved =
            crate::storage::conversations::find_room_by_label_for_user(&ctx.pool, me.id, room_label)
                .await?;
        let (conv_id, conv_channel, room_key) = resolved.ok_or_else(|| {
            anyhow!(
                "no room labeled '{}' that you're a member of (you can only \
                 broadcast to rooms you're in)",
                room_label
            )
        })?;
        if conv_channel != channel_name {
            bail!(
                "room '{}' lives on channel '{}', but you asked to send via '{}'",
                room_label,
                conv_channel,
                channel_name
            );
        }

        let outbound_text = format!("{} wants to tell you all: {}", asker_name, parsed.content);
        channel.send_to_room(&room_key, &outbound_text).await?;

        let logged = format!("[Folkbot proactive — entrusted by {}] {}", asker_name, outbound_text);
        let _ = messages::append_room(&ctx.pool, conv_id, me.id, Role::Assistant, &logged).await;

        tracing::info!(
            "send_message: {} (id={}, asker={}) → room '{}' (conv_id={}, via {}): {} chars",
            me.name,
            me.id,
            asker_name,
            room_label,
            conv_id,
            channel_name,
            parsed.content.chars().count()
        );

        Ok(json!({
            "ok": true,
            "kind": "room",
            "delivered_to_room": room_label,
            "conv_id": conv_id,
            "via": channel_name,
            "room_key": room_key,
            "sent_text": outbound_text,
        }))
    }
}
