//! Channel adapters. Each adapter receives messages from a source (CLI,
//! Telegram, …), constructs a `Principal`, and forwards to `AgentCore`.
//!
//! Channels are also OUTBOUND-capable via the `OutboundChannel` trait —
//! lets the agent proactively message a user (e.g., owner asks Folkbot to
//! relay a reminder). AgentCore holds a registry of available outbound
//! channels; the `send_message` built-in tool dispatches via the registry.

pub mod telegram;

use anyhow::Result;
use async_trait::async_trait;

/// One outbound file to send via [`OutboundChannel::send_file_to_principal`]
/// or [`OutboundChannel::send_file_to_room`]. The path must already exist
/// on disk; sandboxing (i.e. enforcing the path is under `./workspace/`)
/// is the caller's responsibility — the channel just transmits.
pub struct OutboundFile<'a> {
    /// Absolute filesystem path to the file. Channel reads bytes / hands
    /// path to teloxide's `InputFile::file(...)`.
    pub path: std::path::PathBuf,
    /// Original filename (for Telegram display). Defaults to the basename
    /// of `path` if `None`.
    pub display_name: Option<String>,
    /// Mime type (best-effort). Channel uses this to pick the right send_*
    /// method (image/* → send_photo, else → send_document).
    pub mime: &'a str,
    /// Optional Telegram caption (≤1024 chars). Empty / None = no caption.
    pub caption: Option<&'a str>,
}

#[async_trait]
pub trait OutboundChannel: Send + Sync {
    // Channel name as it appears in `user_principals.channel`
    // (e.g. "telegram", "discord"). Used to look up the target user's
    // principal id for this channel.
    // Re-enable when a second channel (discord/line) is added — currently
    // unused because the registry key is the lookup.
    // fn name(&self) -> &str;

    /// Deliver a plain-text message to a principal of this channel.
    /// `principal_id` is the channel-native id (Telegram numeric user id,
    /// Discord snowflake, …).
    async fn send_to_principal(&self, principal_id: &str, content: &str) -> Result<()>;

    /// Deliver a plain-text message to a room (group chat). `room_key` is
    /// the channel-native room id stored in `conversations.room_key` —
    /// for Telegram that's the stringified group chat_id (negative number).
    async fn send_to_room(&self, room_key: &str, content: &str) -> Result<()>;

    /// Deliver a file to a principal. Auto-routes by mime
    /// (image/* → photo, else → document on Telegram).
    async fn send_file_to_principal(
        &self,
        principal_id: &str,
        file: OutboundFile<'_>,
    ) -> Result<()>;

    /// Deliver a file to a room.
    async fn send_file_to_room(&self, room_key: &str, file: OutboundFile<'_>) -> Result<()>;
}
