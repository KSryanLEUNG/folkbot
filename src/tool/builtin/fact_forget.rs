//! `fact_forget` — drop a fact by id (sparingly).

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::ToolSchema;
use crate::storage::facts;
use crate::tool::{Tool, ToolContext};

pub struct FactForget;

#[derive(Deserialize)]
struct FactForgetArgs {
    id: i64,
    /// Why — for audit only, not stored yet (TODO: facts revisions table).
    #[serde(default)]
    reason: Option<String>,
}

#[async_trait]
impl Tool for FactForget {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "fact_forget".into(),
            description:
                "Delete a fact by id. Use sparingly — only when the human explicitly asks to forget \
                 something, or when the fact has clearly become stale (e.g., a past deadline)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The fact id to delete."},
                    "reason": {"type": "string", "description": "Why you're forgetting it."}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let parsed: FactForgetArgs =
            serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {}", e))?;
        let removed = facts::remove(&ctx.pool, parsed.id).await?;
        Ok(json!({
            "ok": removed,
            "id": parsed.id,
            "reason": parsed.reason,
        }))
    }
}
