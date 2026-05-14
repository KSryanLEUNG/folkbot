//! Inbound classification: turn a Telegram `Message` into a `TurnInput` that
//! `AgentCore::run_turn` can consume. Photos / voice / stickers / documents
//! are all routed here; the marker (e.g. `[image: inbox/abc.jpg]`) is what
//! gets persisted, while the bytes ride the wire only for the current turn.

use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;

use crate::agent::{AgentCore, TurnInput};
use crate::media::{self, MediaPart};

/// What `classify_inbound` returns to `handle_message`. `raw_text` is what
/// the user actually typed (without the `[image]` etc. marker prefix) — used
/// for the `/start` slash detection so the prefix doesn't trip it up.
pub(super) struct Intake {
    pub(super) input: TurnInput,
    pub(super) raw_text: String,
}

/// Inspect a Telegram message and produce a `TurnInput` suitable for
/// `AgentCore::run_turn`. Returns `Ok(None)` for messages we can't route
/// (system events, unsupported types with no caption).
///
/// Routing rules:
///   - Plain text → `TurnInput::text_only`
///   - Photo (largest size) → `MediaPart::Image`, marker `[image] caption?`
///   - Sticker (static webp) → `MediaPart::Image`, marker `[sticker emoji?]`
///   - Sticker (animated/video) → `MediaPart::Note`, marker `[animated sticker emoji?]`
///   - Voice → transcribe (best effort) → `MediaPart::Voice`,
///     marker `[voice Ns]`
///   - Document, image-mime → `MediaPart::Image` (treat as photo)
///   - Document, text-mime + small → `MediaPart::TextDoc`
///   - Document, other → `MediaPart::Note`, marker `[file: name (size)]`
pub(super) async fn classify_inbound(
    core: &Arc<AgentCore>,
    bot: &Bot,
    token: &str,
    msg: &Message,
) -> Result<Option<Intake>> {
    if let Some(t) = msg.text() {
        if t.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(Intake {
            input: TurnInput::text_only(t.to_string()),
            raw_text: t.to_string(),
        }));
    }

    let caption = msg.caption().unwrap_or("").trim().to_string();

    // ─── Photo ──────────────────────────────────────────────────
    if let Some(sizes) = msg.photo() {
        let largest = sizes.iter().max_by_key(|p| (p.width as u64) * (p.height as u64));
        if let Some(p) = largest {
            return Ok(Some(
                build_image_intake(bot, token, &p.file, &caption, "image").await,
            ));
        }
    }

    // ─── Sticker ────────────────────────────────────────────────
    if let Some(s) = msg.sticker() {
        let emoji_tag = s
            .emoji
            .as_deref()
            .map(|e| format!(" {}", e))
            .unwrap_or_default();
        if s.is_video() || s.is_animated() {
            let marker = format!("[animated sticker{}]", emoji_tag);
            return Ok(Some(text_marker_intake(marker, &caption)));
        }
        let kind = format!("sticker{}", emoji_tag);
        return Ok(Some(
            build_image_intake(bot, token, &s.file, &caption, &kind).await,
        ));
    }

    // ─── Voice ──────────────────────────────────────────────────
    if let Some(v) = msg.voice() {
        let secs = v.duration.seconds();
        let kind = format!("voice {}s", secs);
        let mime = v
            .mime_type
            .as_ref()
            .map(|m| m.essence_str().to_string())
            .unwrap_or_else(|| "audio/ogg".to_string());
        return Ok(Some(
            build_voice_intake(core, bot, token, &v.file, &mime, &kind, &caption).await,
        ));
    }

    // ─── Audio file (e.g. forwarded mp3) — same treatment as voice ───
    if let Some(a) = msg.audio() {
        let secs = a.duration.seconds();
        let kind = format!("audio {}s", secs);
        let mime = a
            .mime_type
            .as_ref()
            .map(|m| m.essence_str().to_string())
            .unwrap_or_else(|| "audio/mpeg".to_string());
        return Ok(Some(
            build_voice_intake(core, bot, token, &a.file, &mime, &kind, &caption).await,
        ));
    }

    // ─── Document ───────────────────────────────────────────────
    if let Some(d) = msg.document() {
        let filename = d
            .file_name
            .clone()
            .unwrap_or_else(|| "file".to_string());
        let mime = d
            .mime_type
            .as_ref()
            .map(|m| m.essence_str().to_string())
            .unwrap_or_else(|| media::mime_from_filename(&filename).to_string());
        let size_kb = (d.file.size as f64 / 1024.0).round() as u64;

        if media::is_vision_image(&mime) {
            let kind = format!("image file: {}", filename);
            return Ok(Some(
                build_image_intake(bot, token, &d.file, &caption, &kind).await,
            ));
        }
        if media::is_inlineable_text(&mime) && (d.file.size as usize) <= media::MAX_INBOUND_BYTES {
            let kind = format!("file: {} ({}KB)", filename, size_kb);
            return Ok(Some(
                build_textdoc_intake(bot, token, &d.file, &filename, &kind, &caption).await,
            ));
        }
        let kind = format!("file: {} ({}KB · {})", filename, size_kb, mime);
        return Ok(Some(
            build_binary_doc_intake(bot, token, &d.file, &filename, &kind, &caption).await,
        ));
    }

    tracing::debug!("classify_inbound: unsupported message type, skipping");
    Ok(None)
}

/// Compose marker + optional caption into `TurnInput.text`. Used for
/// non-media-bearing intake (notes, animated stickers).
fn text_marker_intake(marker: String, caption: &str) -> Intake {
    let text = if caption.is_empty() {
        marker.clone()
    } else {
        format!("{} {}", marker, caption)
    };
    Intake {
        input: TurnInput {
            text,
            media: vec![],
        },
        raw_text: caption.to_string(),
    }
}

/// Download a Telegram file → save to ./workspace/inbox/ → base64 data URL
/// → `MediaPart::Image`. On any failure, degrades to a `MediaPart::Note`
/// with the marker so the turn still completes.
///
/// The marker text becomes `[image: inbox/<unique_id>.<ext>] caption?` so
/// the LLM can later refer to the saved file via `send_file(workspace_path)`.
async fn build_image_intake(
    bot: &Bot,
    token: &str,
    file_meta: &teloxide::types::FileMeta,
    caption: &str,
    marker_kind: &str,
) -> Intake {
    match fetch_file_bytes(bot, token, &file_meta.id).await {
        Ok((bytes, mime)) => {
            let ext = ext_from_mime(&mime);
            let filename = format!("{}.{}", sanitize_unique_id(&file_meta.unique_id), ext);
            let saved_path = match media::save_to_inbox(&filename, &bytes) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("save image to workspace failed: {:#}", e);
                    None
                }
            };
            let marker = build_marker(marker_kind, saved_path.as_deref());
            let combined_text = compose_marker_caption(&marker, caption);
            let data_url = media::data_url(&mime, &bytes);
            tracing::info!(
                "telegram media: image saved={:?} ({} bytes, mime={})",
                saved_path,
                bytes.len(),
                mime
            );
            Intake {
                input: TurnInput {
                    text: combined_text,
                    media: vec![MediaPart::Image {
                        marker,
                        data_url,
                    }],
                },
                raw_text: caption.to_string(),
            }
        }
        Err(e) => {
            tracing::warn!("image download failed: {:#}", e);
            let marker = build_marker(marker_kind, None);
            let combined_text = compose_marker_caption(&marker, caption);
            Intake {
                input: TurnInput {
                    text: format!("{}(download failed)", combined_text),
                    media: vec![MediaPart::Note {
                        marker: format!("{}(download failed)", marker),
                    }],
                },
                raw_text: caption.to_string(),
            }
        }
    }
}

/// Download voice → save to workspace → optionally transcribe → MediaPart::Voice.
async fn build_voice_intake(
    core: &Arc<AgentCore>,
    bot: &Bot,
    token: &str,
    file_meta: &teloxide::types::FileMeta,
    mime: &str,
    marker_kind: &str,
    caption: &str,
) -> Intake {
    let bytes = match fetch_file_bytes(bot, token, &file_meta.id).await {
        Ok((b, _)) => b,
        Err(e) => {
            tracing::warn!("voice download failed: {:#}", e);
            let marker = build_marker(marker_kind, None);
            let combined_text = compose_marker_caption(&marker, caption);
            return Intake {
                input: TurnInput {
                    text: combined_text,
                    media: vec![MediaPart::Voice {
                        marker,
                        transcript: None,
                    }],
                },
                raw_text: caption.to_string(),
            };
        }
    };

    let ext = ext_from_mime(mime);
    let filename = format!("{}.{}", sanitize_unique_id(&file_meta.unique_id), ext);
    let saved_path = match media::save_to_inbox(&filename, &bytes) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("save voice to workspace failed: {:#}", e);
            None
        }
    };
    let marker = build_marker(marker_kind, saved_path.as_deref());
    let combined_text = compose_marker_caption(&marker, caption);

    // Best-effort transcription. If creds aren't set or call fails, we
    // emit a Voice marker with no transcript and Folkbot replies that it
    // can't hear voice messages yet.
    let transcript = match &core.audio_creds {
        Some(creds) => {
            let trans_filename = format!("voice.{}", ext);
            match crate::llm::audio::transcribe(
                &creds.base_url,
                &creds.api_key,
                &creds.model,
                bytes,
                &trans_filename,
                mime,
            )
            .await
            {
                Ok(t) => {
                    tracing::info!(
                        "voice transcribed: {} chars via {}/{}",
                        t.chars().count(),
                        creds.base_url,
                        creds.model
                    );
                    Some(t)
                }
                Err(e) => {
                    tracing::warn!("voice transcription failed: {:#}", e);
                    None
                }
            }
        }
        None => None,
    };
    Intake {
        input: TurnInput {
            text: combined_text,
            media: vec![MediaPart::Voice { marker, transcript }],
        },
        raw_text: caption.to_string(),
    }
}

/// Download a small text document → save to workspace → inline body into the prompt.
async fn build_textdoc_intake(
    bot: &Bot,
    token: &str,
    file_meta: &teloxide::types::FileMeta,
    original_filename: &str,
    marker_kind: &str,
    caption: &str,
) -> Intake {
    let bytes = match fetch_file_bytes(bot, token, &file_meta.id).await {
        Ok((b, _)) => b,
        Err(e) => {
            tracing::warn!("text doc download failed: {:#}", e);
            let marker = build_marker(marker_kind, None);
            let combined_text = compose_marker_caption(&marker, caption);
            return Intake {
                input: TurnInput {
                    text: format!("{}(download failed)", combined_text),
                    media: vec![MediaPart::Note {
                        marker: format!("{}(download failed)", marker),
                    }],
                },
                raw_text: caption.to_string(),
            };
        }
    };

    let safe_name = media::sanitize_filename(original_filename);
    let stored_filename = format!(
        "{}__{}",
        sanitize_unique_id(&file_meta.unique_id),
        safe_name
    );
    let saved_path = match media::save_to_inbox(&stored_filename, &bytes) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("save text doc to workspace failed: {:#}", e);
            None
        }
    };
    let marker = build_marker(marker_kind, saved_path.as_deref());
    let combined_text = compose_marker_caption(&marker, caption);

    match String::from_utf8(bytes) {
        Ok(body) => {
            tracing::info!(
                "text doc inlined: {} ({} chars)",
                original_filename,
                body.chars().count()
            );
            Intake {
                input: TurnInput {
                    text: combined_text,
                    media: vec![MediaPart::TextDoc {
                        marker,
                        filename: original_filename.to_string(),
                        body,
                    }],
                },
                raw_text: caption.to_string(),
            }
        }
        Err(e) => {
            tracing::warn!("text doc not utf8: {:#}", e);
            Intake {
                input: TurnInput {
                    text: format!("{}(contents are not UTF-8 text)", combined_text),
                    media: vec![MediaPart::Note {
                        marker: format!("{}(not UTF-8)", marker),
                    }],
                },
                raw_text: caption.to_string(),
            }
        }
    }
}

/// Download an unreadable binary doc (PDF, zip, etc) → save to workspace
/// only. The LLM gets a `Note` marker so it can `send_file` to forward,
/// even though it can't read the contents.
async fn build_binary_doc_intake(
    bot: &Bot,
    token: &str,
    file_meta: &teloxide::types::FileMeta,
    original_filename: &str,
    marker_kind: &str,
    caption: &str,
) -> Intake {
    match fetch_file_bytes(bot, token, &file_meta.id).await {
        Ok((bytes, _)) => {
            let safe_name = media::sanitize_filename(original_filename);
            let stored_filename = format!(
                "{}__{}",
                sanitize_unique_id(&file_meta.unique_id),
                safe_name
            );
            let saved_path = match media::save_to_inbox(&stored_filename, &bytes) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("save binary doc to workspace failed: {:#}", e);
                    None
                }
            };
            let marker = build_marker(marker_kind, saved_path.as_deref());
            let combined_text = compose_marker_caption(&marker, caption);
            Intake {
                input: TurnInput {
                    text: combined_text,
                    media: vec![MediaPart::Note { marker }],
                },
                raw_text: caption.to_string(),
            }
        }
        Err(e) => {
            tracing::warn!("binary doc download failed: {:#}", e);
            let marker = build_marker(marker_kind, None);
            text_marker_intake(format!("{}(download failed)", marker), caption)
        }
    }
}

/// Build the marker string the LLM sees in DB / timeline. When we managed
/// to save the bytes, the path is appended so the LLM can refer to it via
/// `send_file(workspace_path)` later.
fn build_marker(kind: &str, saved_path: Option<&str>) -> String {
    match saved_path {
        Some(p) => format!("[{}: {}]", kind, p),
        None => format!("[{}]", kind),
    }
}

fn compose_marker_caption(marker: &str, caption: &str) -> String {
    if caption.is_empty() {
        marker.to_string()
    } else {
        format!("{} {}", marker, caption)
    }
}

/// Telegram's file_unique_id is alphanumeric, but be defensive and strip
/// anything weird before using it as a filename component.
fn sanitize_unique_id(uid: impl std::fmt::Display) -> String {
    media::sanitize_filename(&uid.to_string())
}

/// Map a mime type to a sensible file extension for inbox storage.
/// Falls back to `bin` for unknown types (we still save the file — the
/// LLM can decide whether it's worth sending out).
fn ext_from_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/heic" | "image/heif" => "heic",
        "audio/ogg" | "audio/oga" => "ogg",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/yaml" => "yaml",
        "application/xml" | "text/xml" => "xml",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// Resolve a Telegram `file_id` to (bytes, detected mime).
/// `bot.get_file` returns the relative `path` we then GET via raw HTTP.
async fn fetch_file_bytes(
    bot: &Bot,
    token: &str,
    file_id: &teloxide::types::FileId,
) -> Result<(Vec<u8>, String)> {
    let file = bot
        .get_file(file_id.clone())
        .await
        .map_err(|e| anyhow::anyhow!("get_file: {}", e))?;
    let mime = media::mime_from_filename(&file.path).to_string();
    let bytes = media::download_telegram_file(token, &file.path).await?;
    Ok((bytes, mime))
}
