//! Tool trait + registry.
//!
//! A `Tool` is something the agent can call — `user_identify(name)`, MCP
//! server tools, etc. The registry collects tools and exposes them to the
//! LLM via the `tools` field of the chat completion request.
//!
//! Tools are dispatched in the agent loop in `main::run_chat_turn`:
//! ```ignore
//! let calls = stream.collect_tool_calls().await;
//! for call in calls {
//!     let result = registry.invoke(&call.name, &call.args, &ctx).await?;
//!     history.push(Message::tool(call.id, result));
//! }
//! ```

pub mod builtin;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;

use crate::llm::ToolSchema;
use crate::storage::users::Principal;

/// What a tool needs to do its job.
pub struct ToolContext {
    pub pool: SqlitePool,
    pub principal: Principal,
    /// `None` until the user has been identified. Read by tools that scope
    /// writes (or restrict access) to the current user — `fact_remember`,
    /// `soul_patch`, `cross_user_transcript`, `send_message`.
    pub user_id: Option<i64>,
    /// Outbound channel registry — keys are channel names ("telegram", …).
    /// Used by `send_message` to deliver proactive messages.
    pub outbound: Arc<std::collections::HashMap<String, Arc<dyn crate::channels::OutboundChannel>>>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    async fn invoke(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value>;
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.schema().name.clone();
        self.tools.insert(name, tool);
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    pub async fn invoke(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("unknown tool: {}", name))?;
        tool.invoke(args, ctx).await
    }
}
