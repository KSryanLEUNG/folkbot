//! Best-effort audio transcription via the OpenAI-compatible
//! `/v1/audio/transcriptions` endpoint (Whisper protocol).
//!
//! Used by the Telegram channel when a voice message arrives. If the
//! provider doesn't support this endpoint (Poe's `whisper-1` etc. may or
//! may not be live), the call returns `Err` and the caller falls back to
//! a `MediaPart::Voice { transcript: None, .. }` marker — Folkbot replies
//! "I can't hear voice yet, type please" without crashing the turn.
//!
//! Wire shape: standard OpenAI multipart/form-data:
//!   POST {base_url}/audio/transcriptions
//!   Authorization: Bearer {api_key}
//!   Content-Type: multipart/form-data
//!     file: <bytes>
//!     model: <model_name>
//!   → 200 { "text": "..." }

use anyhow::{anyhow, Context, Result};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: String,
}

/// Hit /v1/audio/transcriptions and return the recognized text.
///
/// Caller should pass the smallest file it can — the multipart upload is
/// the dominant cost for short clips, so we don't bother streaming.
pub async fn transcribe(
    base_url: &str,
    api_key: &str,
    model: &str,
    audio_bytes: Vec<u8>,
    filename: &str,
    mime: &str,
) -> Result<String> {
    let url = format!(
        "{}/audio/transcriptions",
        base_url.trim_end_matches('/')
    );
    let part = Part::bytes(audio_bytes)
        .file_name(filename.to_string())
        .mime_str(mime)
        .map_err(|e| anyhow!("invalid mime '{}': {}", mime, e))?;
    let form = Form::new().part("file", part).text("model", model.to_string());

    let client = Client::builder()
        .timeout(TIMEOUT)
        .build()
        .unwrap_or_else(|_| Client::new());

    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("POST {}", url))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("transcription HTTP {}: {}", status, body));
    }

    let parsed: TranscriptionResponse = resp
        .json()
        .await
        .context("parse transcription response")?;
    Ok(parsed.text)
}
