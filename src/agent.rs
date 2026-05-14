//! Agent core — runs one turn of conversation, regardless of channel.
//!
//! The CLI REPL and the Telegram channel both call `AgentCore::run_turn`. It
//! handles: load history, compose system prompt with token budget, stream LLM
//! response, dispatch tool calls (loop until no more), persist final reply.
//!
//! Channels react to events via `on_event` callback (text deltas, tool start,
//! tool result). CLI prints them; Telegram ignores deltas and waits for the
//! return value (full text).

use anyhow::Result;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::storage::facts::{self, Fact, Importance};
use crate::llm::{ContentPart, ImageUrl, LlmProvider, Message, Role, StreamChunk, ToolCall};
use crate::media::MediaPart;
use crate::storage::messages;
use crate::storage::soul::SoulCard;
use crate::storage::summaries::{self, Period, Summary};
use crate::tokens;
use crate::tool::{ToolContext, ToolRegistry};
use crate::storage::users::{self, Principal, User, UserRole};
use crate::util::{fmt_date, fmt_ts};

const MAX_HISTORY_MSGS: usize = 20;
const RECENT_DAILY_LIMIT: usize = 5;
const SYSTEM_PROMPT_BUDGET: usize = 2000;
const MAX_TOOL_ITERS: usize = 6;

/// Fallback base system prompt when the user hasn't set `[agent]
/// system_prompt` in folkbot.toml. Kept here (the only consumer of base_prompt
/// at runtime) so the config loader, watcher, and CLI all share one source
/// of truth.
pub const DEFAULT_BASE_PROMPT: &str = "You are a family-oriented AI assistant. Keep replies short and natural.";
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
    pub outbound: Arc<std::collections::HashMap<String, Arc<dyn crate::channels::OutboundChannel>>>,
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
            MediaPart::TextDoc { filename, body, .. } => {
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

// ─── System prompt composition ─────────────────────────────────────

/// Per-turn snapshot of all the slow-changing inputs to `build_system_prompt`.
/// Loaded once at turn start and reloaded between tool iterations only when
/// a state-mutating tool ran. Saves ≥4 DB queries per iter when the agent
/// uses non-state-changing tools (MCP, send_message, cross_user_transcript).
struct TurnState {
    soul: SoulCard,
    all_users: Vec<User>,
    user_facts: Vec<Fact>,
    user_summaries: Vec<Summary>,
    room_ctx: Option<RoomCtx>,
    broadcast_targets: Vec<String>,
    /// Cached so the registry doesn't rebuild + re-clone Vec<ToolSchema>
    /// on every iter. Rebuilt only when state reloads — schemas are
    /// effectively static within a turn.
    tool_schemas: Vec<crate::llm::ToolSchema>,
}

impl TurnState {
    async fn load(
        pool: &SqlitePool,
        registry: &ToolRegistry,
        current_user: Option<&User>,
        conv_id: Option<i64>,
    ) -> Result<Self> {
        let soul = SoulCard::load(pool).await?;
        let all_users = users::list_all(pool).await?;
        let user_facts: Vec<Fact> = match current_user {
            Some(u) => facts::list_for_user(pool, u.id, true).await?,
            None => facts::list_for_user(pool, 0, false).await?,
        };
        let user_summaries: Vec<Summary> = match current_user {
            Some(u) => summaries::list_recent(pool, u.id, Period::Day, RECENT_DAILY_LIMIT).await?,
            None => vec![],
        };
        let room_ctx: Option<RoomCtx> = match conv_id {
            Some(cid) => {
                let label = crate::storage::conversations::label(pool, cid).await?;
                let members = crate::storage::conversations::list_members(pool, cid).await?;
                Some(RoomCtx { label, members })
            }
            None => None,
        };
        // Rooms the speaker can broadcast to via `send_message(target_room=...)`.
        // ViceOwner+ only; current room excluded (talk to it directly).
        let broadcast_targets: Vec<String> = match current_user {
            Some(u) if u.role.at_least(UserRole::ViceOwner) => {
                let rooms = crate::storage::conversations::rooms_for_user(pool, u.id).await?;
                rooms
                    .into_iter()
                    .filter(|(rid, _)| Some(*rid) != conv_id)
                    .filter_map(|(_, l)| l)
                    .collect()
            }
            _ => Vec::new(),
        };
        let tool_schemas = registry.schemas();
        Ok(Self {
            soul,
            all_users,
            user_facts,
            user_summaries,
            room_ctx,
            broadcast_targets,
            tool_schemas,
        })
    }
}

/// Per-turn room context, passed into `build_system_prompt` when the turn
/// is happening in a shared group room. `None` everywhere = DM mode.
pub struct RoomCtx {
    pub label: Option<String>,
    pub members: Vec<User>,
}

struct PromptSection {
    priority: u16,
    content: String,
}

/// Compose the system prompt for one chat turn, respecting a token budget.
/// Sections accumulated by priority desc; sections with priority < 95 drop
/// out when budget would be exceeded.
fn build_system_prompt(
    base: &str,
    soul: &SoulCard,
    current: Option<&User>,
    all_users: &[User],
    facts: &[Fact],
    recent_summaries: &[Summary],
    room: Option<&RoomCtx>,
    broadcast_targets: &[String],
    budget_tokens: usize,
) -> String {
    let mut sections: Vec<PromptSection> = Vec::new();

    sections.push(PromptSection { priority: 100, content: base.to_string() });
    sections.push(PromptSection { priority: 99, content: soul.format_for_prompt() });

    // Agent's clock — LLM training data has no concept of "today".
    // Without this, replies about "tomorrow" / "this week" / "last month"
    // anchor against the model's training cutoff, not real time.
    let now_ts = crate::storage::db::now_ts();
    let now_section = format!(
        "\n## Current time\n{} (Asia/Hong_Kong)\n",
        fmt_ts(now_ts)
    );
    sections.push(PromptSection { priority: 97, content: now_section });

    let mut who = String::from("\n## Who I'm talking to\n");
    match current {
        Some(u) => {
            who.push_str(&format!(
                "You're talking to {} (user_id={}, role={}).\n",
                u.display_name.as_deref().unwrap_or(&u.name),
                u.id,
                u.role.as_str()
            ));
            // Spell out the agent's permissions for this user. Without this,
            // the LLM tends to refuse cross-user / privileged actions even
            // when they're authorized at the tool level.
            match u.role {
                UserRole::Owner => who.push_str(
                    "Owner permissions: can `soul_patch` to change my personality, can `cross_user_transcript` \
                     to read my raw conversations with other users, can `send_message` to proactively send messages to other users or groups, \
                     can `send_file` to send files from the workspace to other users or groups. \
                     When the owner asks \"what did you talk about with X\", **proactively call `cross_user_transcript`**; \
                     when the owner says \"tell X / remind X / say to X ...\", **call `send_message` directly \
                     to send it for them** (use `target=username`); \
                     when the owner says \"send X's image / file to Y\", **call `send_file`** \
                     (`workspace_path` comes from the path in the message marker, e.g. `[image: inbox/abc.jpg]` \
                     means use `workspace_path=\"inbox/abc.jpg\"`). \
                     When the owner says \"**show / send / forward to me** that image / file\", also use `send_file`, \
                     `target` is the owner's own name — text replies can't carry files, only `send_file` can. \
                     When the owner says \"say in group X / say in the family group ...\", use `target_room=group_name`. \
                     Just confirm the result with the owner after sending. \
                     Note: `send_message`'s `content` should only contain the body (e.g. \"remember to keep warm\"), \
                     don't start it with \"X wanted me to tell you\" — the tool adds attribution automatically. \
                     `send_file`'s `caption` is body-only too.\n",
                ),
                UserRole::ViceOwner => who.push_str(
                    "Vice owner permissions: can see summary-level overviews of other users (not verbatim), \
                     can `send_message` to ask me to proactively send messages to other users or groups (only groups they've joined), \
                     can `send_file` to send files from the workspace to other users or groups. \
                     Can't change my personality, can't read raw transcripts. When asked for details, honestly say \"only the gist\". \
                     When she says \"tell X / remind X ...\", **call `send_message` directly to send it for her** \
                     (use `target=username`); for \"say in the group\" use `target_room=group_name`; \
                     for \"send that image / file to ...\" use `send_file` (get `workspace_path` from the message marker); \
                     for \"show me\" also use `send_file`, `target` is her own name. \
                     Just confirm the result after sending. \
                     Note: `send_message`'s `content` / `send_file`'s `caption` \
                     should only contain the body (e.g. \"mom's working\"), don't start it with \"X wanted me to tell you\" — \
                     the tool adds attribution automatically.\n",
                ),
                UserRole::Regular => who.push_str(
                    "Regular permissions: you can chat with me and teach me about yourself, \
                     but can't see others or change my personality.\n",
                ),
            }
            // Make broadcast targets explicit so the LLM doesn't have to
            // infer room labels from snapshot section titles. Only shown
            // for owner / vice_owner; shown both in DM and room mode (the
            // current room is filtered out upstream in room mode).
            if u.role.at_least(UserRole::ViceOwner) && !broadcast_targets.is_empty() {
                who.push_str(&format!(
                    "Groups you can broadcast to (use `send_message(target_room=...)`): {}\n",
                    broadcast_targets.join(", ")
                ));
            }
        }
        None => who.push_str(
            "I don't know this person yet. Ask their name naturally in conversation. \
             Once they answer, **call the `user_identify(name)` tool** to remember them; \
             you won't need to ask again.\n",
        ),
    }
    sections.push(PromptSection { priority: 98, content: who });

    // Always tell Folkbot how to read the unified timeline. This applies in
    // both DM and room mode — every prefix in the sliding window now
    // looks like `[time | source] speaker: content`, where source is `DM` or
    // `group "X"`. Folkbot uses the source tag to know where each message
    // happened so it doesn't conflate DM and group context.
    let timeline_note = String::from(
        "\n## How to read the message timeline\n\
         Each message in the sliding window has the format `[time | source] speaker: content`:\n\
         - source = `DM` → DM between that user and you (Folkbot) only\n\
         - source = `group \"X\"` → message in group X, visible to all group members\n\
         Your window is \"the current speaker's personal timeline\": their own DM + all groups they've joined, \
         mixed and ordered by time. That's how you can remember across conversations in real time.\n",
    );
    sections.push(PromptSection { priority: 95, content: timeline_note });

    // Room context: when the turn is happening in a shared group room, list
    // who's in it and remind Folkbot this is a "public hall" — anything said is
    // visible to all members. DM-private secrets shouldn't be re-aired here.
    if let Some(r) = room {
        let mut s = String::from("\n## This conversation space\n");
        match r.label.as_deref() {
            Some(l) => s.push_str(&format!("This reply will be sent into group \"{}\".\n", l)),
            None => s.push_str("This reply will be sent into a group.\n"),
        }
        if !r.members.is_empty() {
            s.push_str("Members:\n");
            for m in &r.members {
                s.push_str(&format!(
                    "- {} (role={})\n",
                    m.display_name.as_deref().unwrap_or(&m.name),
                    m.role.as_str()
                ));
            }
        }
        s.push_str(
            "\n**Important privacy boundary**: your timeline shows the current speaker's DM content, \
             but **this** reply will be seen by all group members. **Don't copy DM content \
             verbatim into a group reply**. You can form judgements based on DM content, but don't quote or paraphrase it. \
             Private promises like \"don't tell X\" you made in DM **especially must not be broken \
             in the group**. No need to prefix replies with `Folkbot:`, just talk as Folkbot.\n",
        );
        sections.push(PromptSection { priority: 96, content: s });
    } else {
        // DM mode reminder: the speaker is talking 1:1 with Folkbot now, but
        // Folkbot may reference (in conversation) the speaker's own group
        // activity from the timeline. Just don't air OTHER people's DM.
        let dm_note = String::from(
            "\n## Where this reply lands\n\
             This reply only goes to the current speaker's DM (private message), nobody else can see it. \
             You can freely discuss their own group activity and their own DM details. \
             But **don't** proactively reveal what other users have told you in their own DMs — that's their private message.\n",
        );
        sections.push(PromptSection { priority: 96, content: dm_note });
    }

    let h_facts: Vec<&Fact> = facts.iter().filter(|f| f.importance == Importance::H).collect();
    if !h_facts.is_empty() {
        let mut s = String::from("\n## Permanent facts ([H], must not forget)\n");
        for f in &h_facts {
            let scope = if f.user_id == 0 { " (shared)" } else { "" };
            s.push_str(&format!("- {}{}\n", f.content, scope));
        }
        sections.push(PromptSection { priority: 90, content: s });
    }

    let m_facts: Vec<&Fact> = facts.iter().filter(|f| f.importance == Importance::M).collect();
    if !m_facts.is_empty() {
        let mut s = String::from("\n## Mid-term facts ([M])\n");
        for f in &m_facts {
            let scope = if f.user_id == 0 { " (shared)" } else { "" };
            s.push_str(&format!("- {}{}\n", f.content, scope));
        }
        sections.push(PromptSection { priority: 75, content: s });
    }

    if !recent_summaries.is_empty() {
        let mut s = String::from("\n## Our recent context\n");
        for sum in recent_summaries {
            s.push_str(&format!(
                "\n### Daily summary ({})\n{}\n",
                fmt_date(sum.start_ts),
                sum.content
            ));
        }
        sections.push(PromptSection { priority: 65, content: s });
    }


    // Cross-user awareness gated by role: only owner + vice_owner see it.
    // Regular users get nothing about other people in their system prompt.
    //
    // Show ALL other users (not just the ones with summaries) so the agent
    // doesn't wrongly claim a user "doesn't exist" when in fact they're in
    // the directory but just haven't chatted enough to have a summary yet.
    let can_see_others = current
        .map(|u| u.role.at_least(UserRole::ViceOwner))
        .unwrap_or(false);
    if can_see_others {
        let others: Vec<&User> = all_users
            .iter()
            .filter(|u| current.map(|c| c.id != u.id).unwrap_or(true))
            .collect();
        if !others.is_empty() {
            let header = if matches!(current.map(|u| u.role), Some(UserRole::Owner)) {
                "\n## Other people I know\n(you're owner, call `cross_user_transcript` when you need the raw text)\n"
            } else {
                "\n## Other people I know (brief — just the gist, no verbatim quoting)\n"
            };
            let mut s = String::from(header);
            for u in others {
                s.push_str(&format!("\n### {} (role={})\n", u.name, u.role.as_str()));
                match (&u.last_summary, u.last_summary_ts) {
                    (Some(sum), Some(ts)) => {
                        s.push_str(&format!("(summary time {})\n{}\n", fmt_ts(ts), sum));
                    }
                    (Some(sum), None) => s.push_str(&format!("{}\n", sum)),
                    (None, _) => s.push_str(
                        "(I haven't really chatted with this person yet, no recent summary — but they are one of the people I know)\n",
                    ),
                }
            }
            sections.push(PromptSection { priority: 50, content: s });
        }
    }

    let l_facts: Vec<&Fact> = facts.iter().filter(|f| f.importance == Importance::L).collect();
    if !l_facts.is_empty() {
        let mut s = String::from("\n## Trivial facts ([L])\n");
        for f in &l_facts {
            let scope = if f.user_id == 0 { " (shared)" } else { "" };
            s.push_str(&format!("- {}{}\n", f.content, scope));
        }
        sections.push(PromptSection { priority: 30, content: s });
    }

    sections.sort_by(|a, b| b.priority.cmp(&a.priority));
    let mut kept = Vec::new();
    let mut used = 0;
    for s in sections {
        let cost = tokens::count(&s.content);
        if s.priority >= 95 || used + cost <= budget_tokens {
            used += cost;
            kept.push(s);
        }
    }
    kept.sort_by(|a, b| b.priority.cmp(&a.priority));
    kept.into_iter().map(|s| s.content).collect::<Vec<_>>().join("\n")
}
