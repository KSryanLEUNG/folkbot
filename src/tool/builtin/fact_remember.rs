//! `fact_remember` — durable memory of things the user said.

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::ToolSchema;
use crate::storage::facts::{self, Importance};
use crate::tool::{Tool, ToolContext};

/// `fact_remember(content, importance, scope?, tags?)` — durable memory.
/// Use when the user states something worth keeping across sessions.
pub struct FactRemember;

#[derive(Deserialize)]
struct FactRememberArgs {
    content: String,
    /// "H" = critical (allergies, deadlines, addresses, hard preferences)
    /// "M" = useful context (habits, projects)
    /// "L" = trivia / colour
    importance: String,
    /// "self" (default — current user) | "shared" (household, user_id 0)
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[async_trait]
impl Tool for FactRemember {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "fact_remember".into(),
            description:
                "Store a durable fact about the human you're talking to (or about the household). \
                 Use when they share something worth remembering across sessions: allergies, \
                 deadlines, preferences, plans, recurring habits. Pick importance carefully — \
                 'H' is for things that should never be forgotten (e.g., 'allergic to peanuts'), \
                 'M' for useful context, 'L' for colour."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The fact, in concise third-person prose."
                    },
                    "importance": {
                        "type": "string",
                        "enum": ["H", "M", "L"],
                        "description": "H=critical/permanent, M=useful, L=trivia"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["self", "shared"],
                        "description": "self = about the current user; shared = about the household. Default 'self'."
                    },
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional tags for later retrieval."
                    }
                },
                "required": ["content", "importance"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let parsed: FactRememberArgs =
            serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {}", e))?;
        let importance = Importance::parse(&parsed.importance)
            .ok_or_else(|| anyhow!("importance must be H, M, or L"))?;
        let scope = parsed.scope.as_deref().unwrap_or("self");
        let owner_id = match scope {
            "shared" => 0,
            "self" => ctx
                .user_id
                .ok_or_else(|| anyhow!("can't store 'self' fact before user is identified"))?,
            other => bail!("scope must be 'self' or 'shared', got '{}'", other),
        };
        let id = facts::add(
            &ctx.pool,
            owner_id,
            importance,
            &parsed.content,
            &parsed.tags,
        )
        .await?;
        Ok(json!({
            "ok": true,
            "id": id,
            "owner_user_id": owner_id,
            "importance": importance.as_str(),
        }))
    }
}
