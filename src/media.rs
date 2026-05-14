//! Media ingestion (v1.4) — photos, voice, stickers, documents.
//!
//! Channel adapters classify inbound non-text content into [`MediaPart`]s and
//! pass them to `AgentCore::run_turn` alongside a text marker. The agent
//! routes vision-capable parts (`Image`) into `Content::Parts` for the LLM
//! call, and persists ONLY the text marker into `messages.content` — bytes
//! are dropped after the turn so DB stays light and future turns see plain
//! text history.
//!
//! The two text fields on [`MediaPart`] serve different purposes:
//! - `marker`: a short tag persisted to DB, e.g. `[image]`, `[voice 5s]`,
//!   `[file: report.pdf (240KB)]`. Visible to Folkbot in future turns.
//! - per-variant payload (data_url, transcript, body): used during the
//!   current turn only. Never persisted as-is.
//!
//! Size cap: 10 MiB per file. Anything bigger is replaced with a marker
//! that says "too large" so Folkbot can respond gracefully instead of OOMing.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

/// Hard cap on a single inbound file. Telegram's bot getFile API itself
/// caps at 20 MiB; we go tighter so base64 payloads (~+33%) plus prompt
/// don't blow the LLM's request size budget.
pub const MAX_INBOUND_BYTES: usize = 10 * 1024 * 1024;

/// Workspace root — relative to folkbot's cwd. Used as both the MCP fs
/// sandbox AND the inbound-media inbox. All file paths handed to
/// the LLM must canonicalize into this directory.
pub const WORKSPACE_DIR: &str = "./workspace";

/// Subdirectory inside the workspace for media saved by `classify_inbound`.
pub const INBOX_SUBDIR: &str = "inbox";

/// Resolve a user-supplied (LLM-supplied) relative path against the
/// workspace root, refusing anything that escapes the sandbox via `..`,
/// symlinks, or absolute paths. Returns the absolute path on success.
///
/// We canonicalize BOTH the workspace root and the candidate target so
/// symlinks-out-of-sandbox are caught regardless of how they're stacked.
pub fn resolve_workspace_path(rel: &str) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::{anyhow, Context};
    use std::path::Path;

    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(anyhow!("path must be relative to workspace, got '{}'", rel));
    }

    let root = Path::new(WORKSPACE_DIR)
        .canonicalize()
        .with_context(|| format!("workspace dir '{}' missing — run from folkbot's cwd", WORKSPACE_DIR))?;
    let candidate = root.join(rel_path);
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("file '{}' not found under workspace", rel))?;
    if !resolved.starts_with(&root) {
        return Err(anyhow!(
            "path '{}' escapes the workspace sandbox",
            rel
        ));
    }
    Ok(resolved)
}

/// Save bytes into `./workspace/inbox/<filename>`. Creates the inbox
/// directory on first use. Returns the path RELATIVE to workspace
/// (e.g. `inbox/abc.jpg`) for embedding in the marker text.
///
/// Idempotent: if the file already exists with non-zero size we skip the
/// write — Telegram's `file_unique_id` is stable per identical content,
/// so re-receiving the same photo doesn't bloat disk.
pub fn save_to_inbox(filename: &str, bytes: &[u8]) -> anyhow::Result<String> {
    use anyhow::Context;
    let inbox = std::path::Path::new(WORKSPACE_DIR).join(INBOX_SUBDIR);
    std::fs::create_dir_all(&inbox).context("create workspace/inbox")?;
    let target = inbox.join(filename);
    if !target.exists() || target.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        std::fs::write(&target, bytes)
            .with_context(|| format!("write inbox/{}", filename))?;
    }
    Ok(format!("{}/{}", INBOX_SUBDIR, filename))
}

/// Sanitize a Telegram-supplied filename so we can store it locally:
/// keep alphanumeric / `.` / `_` / `-`, replace anything else with `_`.
/// Strips path separators (defense against `../etc/passwd`-style names
/// even though we never use the raw input as a path component).
pub fn sanitize_filename(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

/// One inbound media item, paired with the user message in the same turn.
#[derive(Debug, Clone)]
pub enum MediaPart {
    /// Image — sent to vision-capable LLMs as `image_url` content part.
    /// `data_url` is `data:<mime>;base64,...` (LLM accepts this directly).
    /// `marker` is kept around for future channels that may want to
    /// re-emit just the marker (e.g. forwarding the turn to another chat).
    Image {
        #[allow(dead_code)]
        marker: String,
        data_url: String,
    },
    /// Voice / audio note. If transcription succeeded, `transcript` is
    /// `Some(text)` and the LLM gets the transcript inline. If `None`, the
    /// LLM only sees `marker` and Folkbot replies that it can't hear voice.
    Voice {
        #[allow(dead_code)]
        marker: String,
        transcript: Option<String>,
    },
    /// Inlineable text-like document — small text/markdown/csv/json, etc.
    /// The body is wrapped in a fenced code block before being shown to the LLM.
    TextDoc {
        #[allow(dead_code)]
        marker: String,
        filename: String,
        body: String,
    },
    /// Anything we couldn't ingest (binary doc, oversized file, animated
    /// sticker). Just the marker — LLM responds based on the description.
    Note {
        #[allow(dead_code)]
        marker: String,
    },
}

impl MediaPart {
    /// The persisted-to-DB marker (visible in Folkbot's future timeline).
    /// Currently unused — channel handlers compose marker into
    /// `TurnInput.text` directly. Kept as the canonical accessor for any
    /// caller that needs it (debug logs, future re-render paths).
    #[allow(dead_code)]
    pub fn marker(&self) -> &str {
        match self {
            MediaPart::Image { marker, .. } => marker,
            MediaPart::Voice { marker, .. } => marker,
            MediaPart::TextDoc { marker, .. } => marker,
            MediaPart::Note { marker } => marker,
        }
    }
}

/// Build a `data:` URL from raw bytes + mime. Used for vision content parts.
pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{};base64,{}", mime, B64.encode(bytes))
}

/// Best-effort mime detection from a file path / name. Falls back to
/// `application/octet-stream`. Telegram usually sends a real `mime_type`
/// for documents, so this is mostly used for photos (where Telegram only
/// gives us a file_path with .jpg / .webp ext).
pub fn mime_from_filename(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" | "heif" => "image/heic",
        "ogg" | "oga" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "mp4" => "audio/mp4",
        "txt" | "log" | "rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "h" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// True if this mime is something we'll inline into the prompt as text.
/// Conservative list — random binary docs are skipped to avoid garbage.
pub fn is_inlineable_text(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/yaml" | "application/xml"
        )
}

/// True if this mime is something we'll send to a vision LLM. Excludes
/// formats LLMs typically don't handle (heic, bmp depending on provider —
/// but we leave them in; provider will error politely if unsupported).
pub fn is_vision_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/bmp" | "image/heic"
    )
}

/// Download a Telegram file via the public HTTPS endpoint. teloxide's
/// `Bot::download_file` writes to `&mut impl AsyncWrite` — we want raw
/// bytes for base64, so we collect into a Vec instead.
///
/// Returns `Err` if the file exceeds [`MAX_INBOUND_BYTES`] (caller should
/// turn that into a `MediaPart::Note` rather than failing the whole turn).
pub async fn download_telegram_file(
    bot_token: &str,
    file_path: &str,
) -> Result<Vec<u8>> {
    let url = format!("https://api.telegram.org/file/bot{}/{}", bot_token, file_path);
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("download telegram file {}", file_path))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "telegram file fetch HTTP {}",
            resp.status()
        ));
    }
    // Stream the body so we can stop early when the cap is hit.
    let mut bytes = Vec::new();
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read telegram file chunk")?;
        if bytes.len() + chunk.len() > MAX_INBOUND_BYTES {
            return Err(anyhow!(
                "file exceeds {} byte cap",
                MAX_INBOUND_BYTES
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_round_trip() {
        let url = data_url("image/png", &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(url.starts_with("data:image/png;base64,"));
        // 4 bytes → 8 base64 chars (no padding needed for 3-byte multiples,
        // 4 bytes = padding of `=`)
        assert!(url.contains("3q2+7w=="));
    }

    #[test]
    fn mime_detection_handles_common_exts() {
        assert_eq!(mime_from_filename("foo.JPG"), "image/jpeg");
        assert_eq!(mime_from_filename("foo.png"), "image/png");
        assert_eq!(mime_from_filename("voice.ogg"), "audio/ogg");
        assert_eq!(mime_from_filename("notes.md"), "text/markdown");
        assert_eq!(mime_from_filename("data.json"), "application/json");
        assert_eq!(mime_from_filename("nope.xyz"), "application/octet-stream");
        assert_eq!(mime_from_filename("noext"), "application/octet-stream");
    }

    #[test]
    fn vision_filter_excludes_non_image() {
        assert!(is_vision_image("image/jpeg"));
        assert!(is_vision_image("image/png"));
        assert!(!is_vision_image("text/plain"));
        assert!(!is_vision_image("audio/ogg"));
    }

    #[test]
    fn text_inline_filter_correct() {
        assert!(is_inlineable_text("text/plain"));
        assert!(is_inlineable_text("text/markdown"));
        assert!(is_inlineable_text("application/json"));
        assert!(!is_inlineable_text("image/png"));
        assert!(!is_inlineable_text("application/pdf"));
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize_filename("foo.jpg"), "foo.jpg");
        // `..` collapsed via the trim_matches('.') step — extra defense
        // against hidden-file / parent-dir names sneaking through.
        assert_eq!(sanitize_filename("../etc/passwd"), "_etc_passwd");
        assert_eq!(sanitize_filename("photo (1).jpg"), "photo__1_.jpg");
        assert_eq!(sanitize_filename("中文.jpg"), "__.jpg");
    }

    #[test]
    fn workspace_resolve_rejects_absolute() {
        let err = resolve_workspace_path("/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("relative"));
    }

    // Integration-y: requires ./workspace to exist (it does in dev).
    // Verifies that `..` escape is rejected even with a real workspace dir.
    #[test]
    fn workspace_resolve_rejects_escape() {
        // Skip if workspace dir doesn't exist (CI / fresh checkout).
        if !std::path::Path::new(WORKSPACE_DIR).exists() {
            return;
        }
        let err = resolve_workspace_path("../Cargo.toml").unwrap_err();
        // Either "escapes the workspace sandbox" (canonicalize succeeded
        // but landed outside) OR "file not found" (canonicalize tripped) —
        // both are acceptable rejections.
        let msg = err.to_string();
        assert!(
            msg.contains("escapes") || msg.contains("not found"),
            "expected sandbox rejection, got: {}",
            msg
        );
    }
}
