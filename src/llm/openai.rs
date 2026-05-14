//! OpenAI-compatible chat completions client with streaming + tool calling.
//!
//! Tool call streaming is the painful bit. OpenAI emits tool_call deltas in
//! pieces — id arrives in one chunk, function.name in another, then arguments
//! arrive byte-by-byte over many chunks, all keyed by `index`. We accumulate
//! by index and emit the assembled `ToolCalls` once the stream finishes (the
//! human doesn't read tool args streaming, so this is fine UX-wise).

use anyhow::{anyhow, Context, Result};
use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

use super::{LlmProvider, Message, StreamChunk, ToolCall, ToolCallFunction, ToolSchema};

/// Bound on how long we'll wait for the LLM HTTP connect to succeed (TCP +
/// TLS). After this, the request is dropped — provider is treated as
/// unreachable and the turn errors out.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on how long we'll wait between SSE chunks. If the model goes
/// silent for this long mid-stream we assume the connection is dead and
/// abort, rather than blocking the whole turn forever.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct OpenAiCompatProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiCompatProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // Note: NO total `.timeout()` — that would kill long streams.
            // Per-chunk idle timeout in `parse_sse` covers stream stalls.
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
}

/// Streaming response shape (relevant subset).
#[derive(Deserialize, Debug)]
struct StreamingChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize, Debug)]
struct StreamChoice {
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCallDelta>,
}

#[derive(Deserialize, Debug)]
struct StreamToolCallDelta {
    /// OpenAI streams tool_calls keyed by index across chunks.
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize, Debug, Default)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    async fn stream(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolSchema>,
    ) -> Result<BoxStream<'_, Result<StreamChunk>>> {
        let url = format!("{}/chat/completions", self.base_url);

        let tools_wire: Vec<serde_json::Value> = tools
            .into_iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();

        let body = ChatRequest {
            model: &self.model,
            messages: &messages,
            stream: true,
            tools: tools_wire,
        };

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("send chat completion request")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("LLM HTTP {}: {}", status, text));
        }

        let bytes_stream = resp.bytes_stream();
        Ok(Box::pin(parse_sse(bytes_stream)))
    }
}

/// Parse SSE byte stream → emit StreamChunk events.
///
/// State machine: accumulate tool_call deltas by index. Whenever finish_reason
/// arrives, flush the assembled tool_calls (if any) and the Done event.
fn parse_sse(
    bytes_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin + Send + 'static,
) -> impl Stream<Item = Result<StreamChunk>> + Send {
    try_stream! {
        let mut bytes_stream = bytes_stream;
        let mut buf: Vec<u8> = Vec::new();

        // index → partial tool call
        let mut tool_calls_partial: Vec<PartialToolCall> = Vec::new();
        let mut sent_done = false;

        'outer: loop {
            let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, bytes_stream.next())
                .await
                .map_err(|_| anyhow!("LLM stream idle for {}s", STREAM_IDLE_TIMEOUT.as_secs()))?;
            let Some(chunk) = next else { break };
            let chunk = chunk.context("read SSE byte chunk")?;
            buf.extend_from_slice(&chunk);

            loop {
                let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") else {
                    break;
                };
                let event_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let event = std::str::from_utf8(&event_bytes[..pos])
                    .context("SSE event not valid UTF-8")?;

                for line in event.lines() {
                    let Some(data) = line
                        .strip_prefix("data: ")
                        .or_else(|| line.strip_prefix("data:"))
                    else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        if !sent_done {
                            yield StreamChunk::Done;
                        }
                        break 'outer;
                    }
                    if data.is_empty() {
                        continue;
                    }
                    let parsed: StreamingChunk = serde_json::from_str(data)
                        .with_context(|| format!("parse SSE chunk JSON: {}", data))?;
                    let Some(choice) = parsed.choices.into_iter().next() else { continue };

                    // Accumulate tool_call deltas
                    for tcd in choice.delta.tool_calls {
                        while tool_calls_partial.len() <= tcd.index {
                            tool_calls_partial.push(PartialToolCall::default());
                        }
                        let slot = &mut tool_calls_partial[tcd.index];
                        if let Some(id) = tcd.id { slot.id = id; }
                        if let Some(t) = tcd.call_type { slot.call_type = t; }
                        if let Some(f) = tcd.function {
                            if let Some(n) = f.name { slot.name.push_str(&n); }
                            if let Some(a) = f.arguments { slot.arguments.push_str(&a); }
                        }
                    }

                    // Emit text content if any
                    if let Some(text) = choice.delta.content {
                        if !text.is_empty() {
                            yield StreamChunk::Text(text);
                        }
                    }

                    // On finish_reason, flush tool calls then Done
                    if choice.finish_reason.is_some() {
                        if !tool_calls_partial.is_empty() {
                            let calls: Vec<ToolCall> = std::mem::take(&mut tool_calls_partial)
                                .into_iter()
                                .map(|p| p.into_tool_call())
                                .collect();
                            yield StreamChunk::ToolCalls(calls);
                        }
                        yield StreamChunk::Done;
                        sent_done = true;
                        // Don't break — provider will still send `data: [DONE]` after.
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    call_type: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn into_tool_call(self) -> ToolCall {
        ToolCall {
            id: self.id,
            call_type: if self.call_type.is_empty() { "function".into() } else { self.call_type },
            function: ToolCallFunction {
                name: self.name,
                arguments: self.arguments,
            },
        }
    }
}
