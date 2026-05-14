//! LLM provider abstraction with tool calling.
//!
//! `LlmProvider::stream` is the primary method. It takes the message history
//! plus an optional list of tool schemas, and yields a stream of `StreamChunk`
//! events:
//!
//! - `Text(s)`         — incremental assistant text content
//! - `ToolCalls(vec)`  — once per turn (if any), at end of stream, after the
//!                       provider has finished accumulating tool_call deltas
//! - `Done { reason }` — end of stream
//!
//! v0.4 implements the OpenAI-compatible flavor (Poe / Together / Groq /
//! Ollama / OpenRouter / vLLM). Anthropic + native Ollama come later.

pub mod audio;
pub mod openai;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// OpenAI-style `content` field: either a flat string, or an array of typed
/// parts (mixing text + image_url, used for vision turns). Serde untagged
/// so the wire shape matches the spec without manual Serialize impl.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// One element of `Content::Parts`. `type` discriminator + sibling fields
/// matches OpenAI's chat completions multimodal schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    /// Either a public URL or a `data:<mime>;base64,...` URI.
    pub url: String,
}

impl Content {
    /// Convenience constructor — kept for symmetry / future call sites
    /// that build Content without going through Message constructors.
    #[allow(dead_code)]
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text(s.into())
    }
    /// Best-effort plain-text projection, used when the value needs to be
    /// flattened back into a `String` (e.g., persisting to messages.content,
    /// or rendering in `messages list`). For `Parts`, joins all text-typed
    /// parts; image parts are dropped.
    pub fn as_text(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    /// `None` when this is an assistant message that only contains tool_calls.
    /// Serde emits `null` (which is what OpenAI / Anthropic expect).
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(c: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(Content::Text(c.into())),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(Content::Text(c.into())),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    /// User message with mixed text + image parts (vision turn).
    pub fn user_multimodal(parts: Vec<ContentPart>) -> Self {
        Self {
            role: Role::User,
            content: Some(Content::Parts(parts)),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(Content::Text(c.into())),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn tool(call_id: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::Text(result.into())),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String, // always "function" for OpenAI compat
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// JSON-encoded string per OpenAI spec.
    pub arguments: String,
}

/// Schema for a tool advertised to the LLM. Internal representation; converted
/// to provider-specific wire format in each backend impl.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema for the function's parameters.
    pub parameters: serde_json::Value,
}

/// Events yielded by a streaming completion.
#[derive(Debug)]
pub enum StreamChunk {
    Text(String),
    ToolCalls(Vec<ToolCall>),
    /// End-of-stream marker. The provider sends this once per turn (the
    /// `finish_reason` it carries is currently informational only — we
    /// branch on `tool_calls` presence in `agent.rs`, not on the reason).
    Done,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolSchema>,
    ) -> Result<BoxStream<'_, Result<StreamChunk>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_content_serializes_as_string() {
        // OpenAI accepts plain string for the simple case — keeping that
        // shape is important for backward compat with non-vision providers.
        let m = Message::user("hello");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hello");
    }

    #[test]
    fn parts_content_serializes_as_array_of_typed_objects() {
        // Vision turns: content is an array of {type, ...} objects per
        // OpenAI's multimodal schema.
        let m = Message::user_multimodal(vec![
            ContentPart::Text {
                text: "what's in this?".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,AAAA".to_string(),
                },
            },
        ]);
        let v = serde_json::to_value(&m).unwrap();
        let parts = v["content"].as_array().expect("content should be array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what's in this?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn content_as_text_flattens_parts() {
        let c = Content::Parts(vec![
            ContentPart::Text {
                text: "look".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "...".to_string(),
                },
            },
            ContentPart::Text {
                text: "at this".to_string(),
            },
        ]);
        assert_eq!(c.as_text(), "look at this");
    }

    #[test]
    fn assistant_with_tool_calls_serializes_with_null_content() {
        let m = Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: "fact_remember".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            tool_call_id: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        // Null content + populated tool_calls is the OpenAI shape for an
        // assistant turn that's purely a tool call.
        assert!(v["content"].is_null());
        assert_eq!(v["tool_calls"][0]["function"]["name"], "fact_remember");
    }
}
