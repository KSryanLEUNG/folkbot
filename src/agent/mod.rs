//! Agent core — runs one turn of conversation, regardless of channel.
//!
//! The CLI REPL and the Telegram channel both call `AgentCore::run_turn`. It
//! handles: load history, compose system prompt with token budget, stream LLM
//! response, dispatch tool calls (loop until no more), persist final reply.
//!
//! Channels react to events via `on_event` callback (text deltas, tool start,
//! tool result). CLI prints them; Telegram ignores deltas and waits for the
//! return value (full text).

mod prompt;

use prompt::{build_system_prompt, TurnState, SYSTEM_PROMPT_BUDGET};
pub use prompt::DEFAULT_BASE_PROMPT;

use anyhow::Result;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::channels::OutboundChannel;
use crate::llm::{ContentPart, ImageUrl, LlmProvider, Message, Role, StreamChunk, ToolCall};
use crate::media::MediaPart;
use crate::storage::messages;
use crate::storage::summaries;
use crate::storage::users::{self, Principal};
use crate::tool::{ToolContext, ToolRegistry};
use crate::util::fmt_ts;

const MAX_HISTORY_MSGS: usize = 20;
const MAX_TOOL_ITERS: usize = 6;

/// In-room turns trigger a sync `refresh_user_vibe` when this much time
/// has passed between the speaker's last summary and their newest message.
/// Lower = fresher context, more LLM cost. 5 min keeps active sessions
/// from re-summarizing every turn while catching multi-hour lag.
const SUMMARY_STALE_SECS: i64 = 300;

pub struct AgentCore {
    pub pool: SqlitePool,
    pub llm: Arc<dyn LlmProvider>,
    pub registry: ToolRegistry,
    /// Wrapped so the config watcher can swap it out without restarting.
    /// Read path is cheap — we clone the String once per turn.
    pub base_prompt: Arc<tokio::sync::RwLock<String>>,
    /// Outbound channels keyed by name (e.g., "telegram"). Used by the
    /// `send_message` built-in tool to deliver proactive messages.
    pub outbound: Arc<std::collections::HashMap<String, Arc<dyn OutboundChannel>>>,
    /// Best-effort credentials for /v1/audio/transcriptions (whisper).
    /// `None` disables voice transcription — voice messages fall back to a
    /// "I can't hear voice yet" marker. Populated by `bootstrap` from
    /// `[llm]` (reuses base_url + api_key + model="whisper-1") so out of
    /// the box voice works on providers that expose Whisper.
    pub audio_creds: Option<AudioCreds>,
}

/// Shared credentials for the OpenAI-compatible audio transcription
/// endpoint. Cheap to clone (`Arc<String>` would be premature here —
/// `String` is fine since we only clone per voice message).
#[derive(Debug, Clone)]
pub struct AudioCreds {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug)]
pub enum TurnEvent {
    Text(String),
    ToolStart { name: String, args: Value },
    ToolResult {
        /// Tool name. CLI uses this to spot user_identify completions and
        /// react (banner, password challenge); other sinks may inspect it.
        name: String,
        result: Value,
    },
}

/// What a channel hands to `run_turn` for one user interaction.
///
/// `text` is the persisted form (and the historical record): it's what
/// future turns see in the timeline. For media-bearing turns, `text` is
/// usually the marker (e.g. `[image] what is this?`).
///
/// `media` is consumed only for the current turn — bytes never enter the
/// DB. Vision images become `Content::Parts(image_url)`; voice transcripts
/// and text-doc bodies are spliced inline into the prompt.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub text: String,
    pub media: Vec<MediaPart>,
}

impl TurnInput {
    /// Convenience: a plain-text turn (CLI, slash commands, all v1.3 callers).
    pub fn text_only(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            media: Vec::new(),
        }
    }
}

impl AgentCore {
    /// Run one user-facing turn. Persists the user input, drives the
    /// tool-dispatch loop until the agent yields a non-tool reply, persists
    /// that reply, returns the assembled text.
    ///
    /// `conv_id = None` → DM mode (legacy, per-user history).
    /// `conv_id = Some(id)` → room mode (shared history across all members).
    pub async fn run_turn(
        &self,
        principal: &Principal,
        input: TurnInput,
        conv_id: Option<i64>,
        on_event: &mut (dyn FnMut(&TurnEvent) + Send),
    ) -> Result<String> {
        let user_input = input.text.as_str();
        let mut current_user = users::lookup_by_principal(&self.pool, principal).await?;

        // Register speaker as a room member on first contact. Lazy join.
        if let (Some(cid), Some(u)) = (conv_id, current_user.as_ref()) {
            let _ = crate::storage::conversations::add_member(&self.pool, cid, u.id).await;
        }

        // v1.2: when a room turn starts and the speaker's per-user vibe
        // summary is stale (e.g. they DM'd Folkbot minutes ago and the
        // background compressor hasn't ticked yet), refresh it now so the
        // composed prompt reflects their fresh context. Costs ~2-5s on the
        // first response of a turn, but only when actually behind.
        if conv_id.is_some() {
            if let Some(u) = current_user.as_ref() {
                let latest = messages::latest_ts(&self.pool, u.id).await.ok().flatten();
                let stale = match (u.last_summary_ts, latest) {
                    (None, Some(_)) => true,
                    (Some(ls), Some(latest)) => latest.saturating_sub(ls) > SUMMARY_STALE_SECS,
                    _ => false,
                };
                if stale {
                    let _ = summaries::refresh_user_vibe(
                        &self.pool,
                        self.llm.as_ref(),
                        u.id,
                        &u.name,
                    )
                    .await;
                    current_user = users::lookup_by_id(&self.pool, u.id).await?;
                }
            }
        }

        // v1.2.1: unified personal timeline. Folkbot sees the speaker's own DM
        // AND every room they're a member of, merged chronologically. Each
        // message rendered as `[ts | source] speaker: body` so the LLM can
        // distinguish DM-vs-group context and attribute messages correctly.
        let mut history: Vec<Message> = match &current_user {
            Some(u) => {
                let timeline =
                    messages::load_personal_timeline(&self.pool, u.id, MAX_HISTORY_MSGS).await?;
                let conv_ids: Vec<i64> = timeline.iter().map(|(_, _, c, _)| *c).collect();
                let labels =
                    crate::storage::conversations::labels_for(&self.pool, &conv_ids).await?;
                let speaker_ids: Vec<i64> = timeline.iter().map(|(_, _, _, s)| *s).collect();
                let speakers = users::lookup_many_by_ids(&self.pool, &speaker_ids).await?;
                timeline
                    .into_iter()
                    .map(|(mut m, ts, cid, sid)| {
                        // Historical messages are always plain text (any image
                        // bytes were dropped at persist time and only a
                        // `[image] caption` marker remains in messages.content).
                        let body = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                        let source = render_source(cid, &labels);
                        let prefixed = match m.role {
                            Role::User => {
                                let speaker = speakers
                                    .get(&sid)
                                    .map(|u| u.name.as_str())
                                    .unwrap_or("?");
                                format!("[{} | {}] {}: {}", fmt_ts(ts), source, speaker, body)
                            }
                            _ => format!("[{} | {}] Folkbot: {}", fmt_ts(ts), source, body),
                        };
                        m.content = Some(crate::llm::Content::Text(prefixed));
                        m
                    })
                    .collect()
            }
            None => vec![],
        };

        // Persist user input as soon as we know who they are. If unidentified,
        // hold off — `user_identify` may run mid-turn and tell us. We come
        // back and persist retroactively after each tool call (see below).
        let mut user_msg_persisted = false;
        if let Some(u) = &current_user {
            match conv_id {
                Some(cid) => {
                    let _ =
                        messages::append_room(&self.pool, cid, u.id, Role::User, user_input).await;
                }
                None => {
                    let _ = messages::append(&self.pool, u.id, Role::User, user_input).await;
                }
            }
            user_msg_persisted = true;
        }
        // v1.2.1: tag the current user input with the same `[ts | source]
        // speaker:` format as historical messages so Folkbot reads it as a
        // continuous timeline. The source tells Folkbot whether this turn is
        // happening in DM (private 1:1) or in a specific group room.
        let prefixed_text = match current_user.as_ref() {
            Some(u) => {
                let now = crate::storage::db::now_ts();
                let source = match conv_id {
                    None => "DM".to_string(),
                    Some(cid) => match crate::storage::conversations::label(&self.pool, cid).await? {
                        Some(l) => format!("group \"{}\"", l),
                        None => format!("group#{}", cid),
                    },
                };
                format!("[{} | {}] {}: {}", fmt_ts(now), source, u.name, user_input)
            }
            None => user_input.to_string(),
        };

        // v1.4: fold media into the wire user message.
        //   - Image parts become Content::Parts entries (LLM sees them).
        //   - Voice transcripts + text-doc bodies are spliced into the text
        //     part inline (`(voice transcript: ...)` / fenced code block).
        //   - Marker is already in `prefixed_text` (channel set input.text
        //     to `[image] caption?` etc), so the LLM always knows there was
        //     an attachment even if it can't see the bytes.
        history.push(build_wire_user_message(&prefixed_text, &input.media));

        let mut final_text = String::new();

        // Per-turn snapshot of context the system prompt depends on.
        // Loaded once at the start of the turn; only reloaded across tool
        // iterations when a tool ran that could have mutated this state.
        // Worth caching: the typical multi-iter turn uses non-state-changing
        // tools (MCP fetch, send_message, cross_user_transcript) — without
        // the cache we'd re-query soul+users+facts+summaries per iter.
        let mut state = TurnState::load(&self.pool, &self.registry, current_user.as_ref(), conv_id).await?;

        for _iter in 0..MAX_TOOL_ITERS {
            let base = self.base_prompt.read().await.clone();
            let composed_system = build_system_prompt(
                &base,
                &state.soul,
                current_user.as_ref(),
                &state.all_users,
                &state.user_facts,
                &state.user_summaries,
                state.room_ctx.as_ref(),
                &state.broadcast_targets,
                SYSTEM_PROMPT_BUDGET,
            );

            let mut wire = Vec::with_capacity(history.len() + 1);
            wire.push(Message::system(composed_system));
            wire.extend(history.iter().cloned());

            let mut stream = self.llm.stream(wire, state.tool_schemas.clone()).await?;
            let mut iter_text = String::new();
            let mut tool_calls: Vec<ToolCall> = vec![];

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(StreamChunk::Text(t)) => {
                        on_event(&TurnEvent::Text(t.clone()));
                        iter_text.push_str(&t);
                    }
                    Ok(StreamChunk::ToolCalls(c)) => tool_calls = c,
                    Ok(StreamChunk::Done) => break,
                    Err(e) => return Err(e),
                }
            }

            final_text.push_str(&iter_text);

            if tool_calls.is_empty() {
                if let Some(u) = &current_user {
                    if !iter_text.trim().is_empty() {
                        match conv_id {
                            Some(cid) => {
                                let _ = messages::append_room(
                                    &self.pool,
                                    cid,
                                    u.id,
                                    Role::Assistant,
                                    &iter_text,
                                )
                                .await;
                            }
                            None => {
                                let _ = messages::append(
                                    &self.pool,
                                    u.id,
                                    Role::Assistant,
                                    &iter_text,
                                )
                                .await;
                            }
                        }
                    }
                }
                return Ok(final_text);
            }

            // Has tool calls — push assistant_with_calls + dispatch each.
            history.push(Message {
                role: Role::Assistant,
                content: if iter_text.is_empty() {
                    None
                } else {
                    Some(crate::llm::Content::Text(iter_text))
                },
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
            });

            // Track whether any tool ran that mutated state we depend on.
            // If so, we'll reload `state` once at the end of this iteration
            // (cheaper than per-iter reload of unchanged tools' worth).
            let mut state_dirty = false;
            for call in tool_calls {
                let args: Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or(Value::Object(Default::default()));
                on_event(&TurnEvent::ToolStart {
                    name: call.function.name.clone(),
                    args: args.clone(),
                });

                let ctx = ToolContext {
                    pool: self.pool.clone(),
                    principal: principal.clone(),
                    user_id: current_user.as_ref().map(|u| u.id),
                    outbound: self.outbound.clone(),
                };
                let result = match self.registry.invoke(&call.function.name, args, &ctx).await {
                    Ok(v) => v,
                    Err(e) => json!({"error": e.to_string()}),
                };
                on_event(&TurnEvent::ToolResult {
                    name: call.function.name.clone(),
                    result: result.clone(),
                });
                history.push(Message::tool(
                    call.id,
                    serde_json::to_string(&result).unwrap_or_default(),
                ));

                // Tools that mutate state read by build_system_prompt.
                // Other tools (cross_user_transcript, send_message, MCP) leave
                // state alone — we keep the cached snapshot in those cases.
                if matches!(
                    call.function.name.as_str(),
                    "user_identify" | "soul_patch" | "fact_remember" | "fact_forget"
                ) {
                    state_dirty = true;
                }

                if call.function.name == "user_identify" {
                    current_user = users::lookup_by_principal(&self.pool, principal).await?;
                    // Retroactive persist: if user input was unidentified at
                    // turn start, we held off writing it. Now that we know who
                    // they are, write it under the freshly-linked user_id.
                    if !user_msg_persisted {
                        if let Some(u) = &current_user {
                            match conv_id {
                                Some(cid) => {
                                    let _ = crate::storage::conversations::add_member(
                                        &self.pool,
                                        cid,
                                        u.id,
                                    )
                                    .await;
                                    let _ = messages::append_room(
                                        &self.pool,
                                        cid,
                                        u.id,
                                        Role::User,
                                        user_input,
                                    )
                                    .await;
                                }
                                None => {
                                    let _ = messages::append(
                                        &self.pool,
                                        u.id,
                                        Role::User,
                                        user_input,
                                    )
                                    .await;
                                }
                            }
                            user_msg_persisted = true;
                        }
                    }
                }
            }

            // Reload turn state if anything we cached might have changed.
            // No-op when the iteration only invoked side-effect-only tools.
            if state_dirty {
                state = TurnState::load(
                    &self.pool,
                    &self.registry,
                    current_user.as_ref(),
                    conv_id,
                )
                .await?;
            }
        }

        // Tool-iteration ceiling hit. Don't leave the user staring at a half-
        // finished response — surface a short note (so CLI / Telegram show
        // *something*), persist it as Folkbot's reply, and log the bail-out.
        let exhaust_note = "(I called tools too many times this turn, stopping here. Try rephrasing and ask again.)";
        on_event(&TurnEvent::Text(exhaust_note.to_string()));
        final_text.push_str(exhaust_note);
        if let Some(u) = &current_user {
            match conv_id {
                Some(cid) => {
                    let _ =
                        messages::append_room(&self.pool, cid, u.id, Role::Assistant, exhaust_note)
                            .await;
                }
                None => {
                    let _ = messages::append(&self.pool, u.id, Role::Assistant, exhaust_note).await;
                }
            }
        }
        tracing::warn!(
            "run_turn: MAX_TOOL_ITERS ({}) exhausted for principal {}",
            MAX_TOOL_ITERS,
            principal.label()
        );
        Ok(final_text)
    }
}

impl AgentCore {
    /// Compose what `run_turn` would send as the system prompt for a given
    /// user — useful for `folkbot prompt` debugging without actually calling
    /// the LLM. `user_id = None` means "unidentified" (shows the same
    /// prompt a stranger would see). `conv_id = Some(_)` renders the
    /// room-mode variant (members + public-hall reminder).
    pub async fn compose_system_prompt_for(
        &self,
        user_id: Option<i64>,
        conv_id: Option<i64>,
    ) -> Result<String> {
        let current = match user_id {
            Some(uid) => users::lookup_by_id(&self.pool, uid).await?,
            None => None,
        };
        let state = TurnState::load(&self.pool, &self.registry, current.as_ref(), conv_id).await?;
        let base = self.base_prompt.read().await.clone();
        Ok(build_system_prompt(
            &base,
            &state.soul,
            current.as_ref(),
            &state.all_users,
            &state.user_facts,
            &state.user_summaries,
            state.room_ctx.as_ref(),
            &state.broadcast_targets,
            SYSTEM_PROMPT_BUDGET,
        ))
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

/// Build the wire user `Message` for the current turn, weaving any media
/// parts into the prompt.
///
/// `prefixed_text` already contains the `[ts | source] speaker: marker`
/// header (set up by `run_turn`). We append voice transcripts and text-doc
/// bodies to that text, and pull image data URLs into a separate
/// `image_url` part so the LLM's vision pipeline picks them up.
///
/// When `media` has zero image parts the result is a plain `Content::Text`,
/// keeping the wire indistinguishable from a v1.3-era message.
fn build_wire_user_message(prefixed_text: &str, media: &[MediaPart]) -> Message {
    let mut text_buf = prefixed_text.to_string();
    let mut image_parts: Vec<ContentPart> = Vec::new();

    for m in media {
        match m {
            MediaPart::Image { data_url, .. } => {
                image_parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: data_url.clone(),
                    },
                });
            }
            MediaPart::Voice {
                transcript: Some(t),
                ..
            } => {
                text_buf.push_str(&format!("\n(voice transcript: {})", t.trim()));
            }
            MediaPart::Voice {
                transcript: None, ..
            } => {
                // Marker already in prefixed_text; nothing to inline.
            }
            MediaPart::TextDoc {
                filename, body, ..
            } => {
                text_buf.push_str(&format!(
                    "\n\nattachment `{}` contents:\n```\n{}\n```",
                    filename,
                    body.trim_end()
                ));
            }
            MediaPart::Note { .. } => {
                // Marker conveys everything; no inline payload.
            }
        }
    }

    if image_parts.is_empty() {
        Message::user(text_buf)
    } else {
        let mut parts = vec![ContentPart::Text { text: text_buf }];
        parts.extend(image_parts);
        Message::user_multimodal(parts)
    }
}

/// Render the conversation-source tag used in sliding-window prefixes.
/// `0` → `DM`, otherwise the room's label (or `group#<id>` if unlabeled).
fn render_source(
    conv_id: i64,
    labels: &std::collections::HashMap<i64, Option<String>>,
) -> String {
    if conv_id == 0 {
        return "DM".to_string();
    }
    match labels.get(&conv_id).and_then(|o| o.as_deref()) {
        Some(l) => format!("group \"{}\"", l),
        None => format!("group#{}", conv_id),
    }
}
