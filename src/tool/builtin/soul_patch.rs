//! `soul_patch` — owner-only edit of Folkbot's own identity (the soul card).

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::ToolSchema;
use crate::storage::soul::{self, Field, Op, Patch};
use crate::storage::users::{self, UserRole};
use crate::tool::{Tool, ToolContext};

/// `soul_patch` lets the agent update its OWN identity (the soul card).
/// Owner-restricted: only `users.role = 'owner'` users can trigger this.
/// Same patch validation as the CLI: per-patch char cap, per-day cap,
/// per-field cooldown, locked fields rejected.
pub struct SoulPatch;

#[derive(Deserialize)]
struct SoulPatchArgs {
    /// One of: "core_values", "tone", "traits", "quirks", "nicknames",
    /// "formative_memories", "people". `name` and `kind` are locked by
    /// default and only editable via CLI unlock.
    field: String,
    /// "add" | "modify" | "remove"
    op: String,
    /// For string-list fields (`traits`, `quirks`, `nicknames`,
    /// `formative_memories`): the entry text. For `people`: a JSON object
    /// {"name":"…","relationship":"…","key_memories":["…"]}. For scalars
    /// (`core_values`, `tone`): the new full value (use op="modify").
    content: String,
    /// Why you're making this change. Required, audited via
    /// `folkbot soul history`.
    reason: String,
}

#[async_trait]
impl Tool for SoulPatch {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "soul_patch".into(),
            description:
                "Update YOUR OWN identity (your nickname, personality traits, tone, formative \
                 memories, etc.). Use when the human teaches you about who you are or how you \
                 should behave. Each patch is small (≤500 chars, slow-drift fields ≤200) and \
                 audited; you can't dramatically rewrite yourself in one go. ONLY OWNERS can \
                 call this — for non-owner users, the call will fail and you should respond \
                 honestly that you can't change your own identity for this person."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "field": {
                        "type": "string",
                        "enum": [
                            "core_values", "tone", "traits", "people",
                            "quirks", "formative_memories", "nicknames"
                        ],
                        "description": "Which part of your identity to modify."
                    },
                    "op": {
                        "type": "string",
                        "enum": ["add", "modify", "remove"],
                        "description": "add: append to a list / append to a string. modify: replace scalar value. remove: drop a list entry by exact match."
                    },
                    "content": {
                        "type": "string",
                        "description": "The new value. For lists, a single entry. For `people`, a JSON object string."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Brief why — audited."
                    }
                },
                "required": ["field", "op", "content", "reason"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let parsed: SoulPatchArgs =
            serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {}", e))?;

        // Owner check.
        let uid = ctx
            .user_id
            .ok_or_else(|| anyhow!("can't modify my identity before identification"))?;
        let user = users::lookup_by_id(&ctx.pool, uid)
            .await?
            .ok_or_else(|| anyhow!("user_id {} not found", uid))?;
        if user.role != UserRole::Owner {
            bail!(
                "only owner can change my identity. {} is {}, no permission. \
                 (to set role, run `folkbot user set-role <name> owner` from the CLI)",
                user.name,
                user.role.as_str()
            );
        }

        let field = Field::parse(&parsed.field)
            .ok_or_else(|| anyhow!("unknown field '{}'", parsed.field))?;
        let op = Op::parse(&parsed.op)
            .ok_or_else(|| anyhow!("unknown op '{}'", parsed.op))?;

        let rev = soul::apply_patch(
            &ctx.pool,
            Patch {
                field,
                op,
                content: parsed.content,
                reason: parsed.reason,
            },
        )
        .await?;

        Ok(json!({
            "ok": true,
            "revision": rev,
            "field": field.as_str(),
        }))
    }
}
