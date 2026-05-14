//! `send_file` — owner/vice_owner outbound file delivery from `./workspace/`.

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::SendRateLimit;
use crate::llm::{Role, ToolSchema};
use crate::storage::messages;
use crate::storage::users::{self, UserRole};
use crate::tool::{Tool, ToolContext};

/// `send_file` lets the agent forward a file from `./workspace/` to a user
/// or room. Sandbox-checked: paths must canonicalize under the workspace
/// root or the call is rejected. Auto-routes by mime: image/* → photo
/// (compressed in-line preview), else → document.
///
/// Owner + vice_owner only. Shares rate limit with `send_message`.
pub struct SendFile {
    rate: std::sync::Arc<SendRateLimit>,
}

impl SendFile {
    pub fn new(rate: std::sync::Arc<SendRateLimit>) -> Self {
        Self { rate }
    }
}

#[derive(Deserialize)]
struct SendFileArgs {
    /// Target user name (case-insensitive). Mutually exclusive with `target_room`.
    #[serde(default)]
    target: Option<String>,
    /// Target room label. Mutually exclusive with `target`. Asker must be a member.
    #[serde(default)]
    target_room: Option<String>,
    /// Workspace-relative path, e.g. `inbox/abc.jpg` or `holiday/img.png`.
    /// Must canonicalize under `./workspace/` — paths escaping the sandbox
    /// (`../...`, absolute paths, symlinks-out) are rejected.
    workspace_path: String,
    /// Optional caption shown alongside the file in Telegram.
    #[serde(default)]
    caption: Option<String>,
    /// Channel to deliver via. Default "telegram".
    #[serde(default)]
    channel: Option<String>,
    /// Override the auto-prefix asker name. Defaults to YOUR own name.
    #[serde(default)]
    from_name: Option<String>,
}

#[async_trait]
impl Tool for SendFile {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "send_file".into(),
            description:
                "Send a FILE from your workspace to another USER (DM) or to a ROOM. \
                 Allowed for OWNER and VICE_OWNER — regular users can't trigger this. \
                 The file must already be under `./workspace/` (Folkbot's sandbox); typical \
                 source is `inbox/...` for files received via Telegram earlier (their \
                 path is shown in the marker, e.g. `[image: inbox/abc.jpg]`). \
                 Auto-detects kind: image files go as photos, everything else as documents. \
                 Provide EITHER `target` OR `target_room`, not both. \
                 The `caption` is shown with the file in Telegram (≤1024 chars)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "User name to DM. Mutually exclusive with target_room."
                    },
                    "target_room": {
                        "type": "string",
                        "description": "Room label. Asker must be a member."
                    },
                    "workspace_path": {
                        "type": "string",
                        "description": "Workspace-relative path, e.g. 'inbox/abc.jpg'. Must be under ./workspace/."
                    },
                    "caption": {
                        "type": "string",
                        "description": "Optional caption shown with the file."
                    },
                    "channel": {
                        "type": "string",
                        "description": "Channel name; defaults to 'telegram'.",
                        "default": "telegram"
                    },
                    "from_name": {
                        "type": "string",
                        "description": "Override prefix asker name. Defaults to your own."
                    }
                },
                "required": ["workspace_path"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let parsed: SendFileArgs =
            serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {}", e))?;

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
            (Some(_), Some(_)) => bail!("specify only one of `target` and `target_room`"),
            _ => {}
        }

        let uid = ctx.user_id.ok_or_else(|| anyhow!("not identified"))?;
        let me = users::lookup_by_id(&ctx.pool, uid)
            .await?
            .ok_or_else(|| anyhow!("user_id {} not found", uid))?;
        if !me.role.at_least(UserRole::ViceOwner) {
            bail!(
                "regular users can't send files. You are {} ({}).",
                me.name,
                me.role.as_str()
            );
        }

        self.rate.check_and_record(me.id, &me.name).await?;

        // Sandbox path resolution — reject anything escaping ./workspace/.
        let abs_path = crate::media::resolve_workspace_path(&parsed.workspace_path)?;
        if !abs_path.is_file() {
            bail!("not a regular file: {}", parsed.workspace_path);
        }
        let display_name = abs_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let mime = crate::media::mime_from_filename(
            display_name.as_deref().unwrap_or(&parsed.workspace_path),
        );

        let channel_name = parsed.channel.clone().unwrap_or_else(|| "telegram".into());
        let channel = ctx
            .outbound
            .get(&channel_name)
            .ok_or_else(|| anyhow!("channel '{}' not configured", channel_name))?
            .clone();

        let asker_name = parsed.from_name.as_deref().unwrap_or(&me.name);
        let caption_text = parsed.caption.as_deref().unwrap_or("").trim().to_string();

        let outbound_file = crate::channels::OutboundFile {
            path: abs_path.clone(),
            display_name: display_name.clone(),
            mime,
            caption: if caption_text.is_empty() {
                None
            } else {
                Some(caption_text.as_str())
            },
        };

        // ─── Branch 1: DM ────────────────────────────────────────
        if let Some(target_name) = target {
            let target = users::lookup_by_name(&ctx.pool, target_name)
                .await?
                .ok_or_else(|| anyhow!("no user named '{}'", target_name))?;
            // Note: target == me IS allowed for send_file (unlike send_message),
            // because the normal reply path is text-only — there's no other way
            // for Folkbot to push a file back to the asker's own DM.

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

            channel
                .send_file_to_principal(&principal_id, outbound_file)
                .await?;

            let logged = format!(
                "[Folkbot proactive — entrusted by {}] sent {} to {}{}",
                asker_name,
                parsed.workspace_path,
                target.name,
                if caption_text.is_empty() {
                    String::new()
                } else {
                    format!(" (caption: {})", caption_text)
                }
            );
            let _ = messages::append(&ctx.pool, target.id, Role::Assistant, &logged).await;

            tracing::info!(
                "send_file: {} → DM {} ({}, mime={}, {} bytes)",
                me.name,
                target.name,
                parsed.workspace_path,
                mime,
                std::fs::metadata(&abs_path).map(|m| m.len()).unwrap_or(0)
            );

            return Ok(json!({
                "ok": true,
                "kind": "dm",
                "delivered_to": target.name,
                "via": channel_name,
                "workspace_path": parsed.workspace_path,
                "mime": mime,
            }));
        }

        // ─── Branch 2: Room ─────────────────────────────────────
        let room_label = target_room.unwrap();
        let resolved =
            crate::storage::conversations::find_room_by_label_for_user(&ctx.pool, me.id, room_label)
                .await?;
        let (conv_id, conv_channel, room_key) = resolved.ok_or_else(|| {
            anyhow!(
                "no room labeled '{}' that you're a member of",
                room_label
            )
        })?;
        if conv_channel != channel_name {
            bail!(
                "room '{}' lives on channel '{}', not '{}'",
                room_label,
                conv_channel,
                channel_name
            );
        }

        channel
            .send_file_to_room(&room_key, outbound_file)
            .await?;

        let logged = format!(
            "[Folkbot proactive — entrusted by {}] sent {} to group \"{}\"{}",
            asker_name,
            parsed.workspace_path,
            room_label,
            if caption_text.is_empty() {
                String::new()
            } else {
                format!(" (caption: {})", caption_text)
            }
        );
        let _ = messages::append_room(&ctx.pool, conv_id, me.id, Role::Assistant, &logged).await;

        tracing::info!(
            "send_file: {} → room '{}' ({}, mime={}, {} bytes)",
            me.name,
            room_label,
            parsed.workspace_path,
            mime,
            std::fs::metadata(&abs_path).map(|m| m.len()).unwrap_or(0)
        );

        Ok(json!({
            "ok": true,
            "kind": "room",
            "delivered_to_room": room_label,
            "conv_id": conv_id,
            "via": channel_name,
            "workspace_path": parsed.workspace_path,
            "mime": mime,
        }))
    }
}
