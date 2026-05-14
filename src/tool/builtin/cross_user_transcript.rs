//! `cross_user_transcript` — owner-only read of another user's raw messages.

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::{Role, ToolSchema};
use crate::storage::messages;
use crate::storage::users::{self, UserRole};
use crate::tool::{Tool, ToolContext};

/// `cross_user_transcript` lets the owner read another user's RAW recent
/// messages. This is the only path to literal cross-user transcripts; vice
/// owners and regular users can't call it. Use sparingly — surfacing raw
/// chat to a third party is sensitive even when permitted.
pub struct CrossUserTranscript;

#[derive(Deserialize)]
struct CrossUserArgs {
    user_name: String,
    #[serde(default = "default_last_n")]
    last_n: usize,
}

fn default_last_n() -> usize {
    20
}

#[async_trait]
impl Tool for CrossUserTranscript {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "cross_user_transcript".into(),
            description:
                "Fetch RAW recent messages between you and another user. OWNER ONLY. \
                 Use only when the owner explicitly asks to review what someone said \
                 (e.g., \"what did Lucky tell you yesterday\"). Do NOT call casually. \
                 If asked for a summary instead, prefer answering from the \
                 cross-user summaries already in your context."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "user_name": {"type": "string", "description": "Name of the user whose messages to fetch."},
                    "last_n": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
                },
                "required": ["user_name"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let parsed: CrossUserArgs =
            serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {}", e))?;

        let uid = ctx
            .user_id
            .ok_or_else(|| anyhow!("not identified"))?;
        let me = users::lookup_by_id(&ctx.pool, uid)
            .await?
            .ok_or_else(|| anyhow!("user_id {} not found", uid))?;
        if me.role != UserRole::Owner {
            bail!(
                "only owner can read raw cross-user transcripts. \
                 You are {} ({}). Tell the asker honestly that only the \
                 household owner has this access.",
                me.name,
                me.role.as_str()
            );
        }

        let target = users::lookup_by_name(&ctx.pool, &parsed.user_name)
            .await?
            .ok_or_else(|| anyhow!("no user named '{}'", parsed.user_name))?;

        if target.id == me.id {
            // Self-fetch via this tool is pointless — own messages are in
            // the prompt already. Tell the agent.
            bail!("you are asking for your own transcript; use your existing context");
        }

        tracing::info!(
            "cross_user_transcript: owner {} (id={}) reading {} (id={}, last_n={})",
            me.name,
            me.id,
            target.name,
            target.id,
            parsed.last_n
        );

        // v1.3: span DM + rooms so the owner sees the user's full picture,
        // not just 1:1 history. Each entry is tagged with its source so the
        // owner can tell where each line happened.
        let msgs = messages::load_last_n_for_user_all_convs(
            &ctx.pool,
            target.id,
            parsed.last_n,
        )
        .await?;
        let conv_ids: Vec<i64> = msgs.iter().map(|(_, _, c)| *c).collect();
        let labels = crate::storage::conversations::labels_for(&ctx.pool, &conv_ids).await?;

        let json_msgs: Vec<_> = msgs
            .iter()
            .map(|(m, ts, cid)| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    _ => "other",
                };
                let source = if *cid == 0 {
                    "DM".to_string()
                } else {
                    match labels.get(cid).and_then(|o| o.as_deref()) {
                        Some(l) => format!("group \"{}\"", l),
                        None => format!("group#{}", cid),
                    }
                };
                json!({
                    "role": role,
                    "ts": ts,
                    "ts_formatted": crate::util::fmt_ts(*ts),
                    "source": source,
                    "content": m.content.as_ref().map(|c| c.as_text()).unwrap_or_default(),
                })
            })
            .collect();

        Ok(json!({
            "user": target.name,
            "user_id": target.id,
            "count": json_msgs.len(),
            "messages": json_msgs,
        }))
    }
}
