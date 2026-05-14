//! Outbound-only Telegram client. Wraps a `Bot` (cheap to clone — internally
//! an Arc) so it can be shared with the inbound polling task without
//! sequencing concerns: the Telegram Bot API allows many concurrent
//! `sendMessage` calls per token, only `getUpdates` is single-client.

use anyhow::Result;
use async_trait::async_trait;
use teloxide::prelude::*;
use teloxide::types::ChatId;

use super::streaming::chunk_for_telegram;
use crate::channels::{OutboundChannel, OutboundFile};

pub struct TelegramOutbound {
    bot: Bot,
}

impl TelegramOutbound {
    pub fn from_token(token: String) -> Self {
        Self { bot: Bot::new(token) }
    }

    /// Internal: send to any chat_id. Both DM principal_ids (positive) and
    /// group room_keys (negative) parse to a `ChatId` the same way.
    async fn send_to_chat(&self, chat_id_str: &str, content: &str) -> Result<()> {
        let chat_id: i64 = chat_id_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid telegram chat id '{}': {}", chat_id_str, e))?;
        for chunk in chunk_for_telegram(content) {
            self.bot
                .send_message(ChatId(chat_id), chunk)
                .await
                .map_err(|e| anyhow::anyhow!("telegram send: {}", e))?;
        }
        Ok(())
    }

    /// Internal: route a file to any chat_id. mime decides whether
    /// Telegram should treat it as a photo (compressed, in-line preview)
    /// or a document (preserves bytes, opens in a viewer).
    async fn send_file_to_chat(
        &self,
        chat_id_str: &str,
        file: OutboundFile<'_>,
    ) -> Result<()> {
        use teloxide::types::InputFile;
        let chat_id: i64 = chat_id_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid telegram chat id '{}': {}", chat_id_str, e))?;
        let chat = ChatId(chat_id);

        let mut input = InputFile::file(&file.path);
        if let Some(name) = file.display_name.as_deref() {
            input = input.file_name(name.to_string());
        }

        // Auto kind by mime: image/* → send_photo (compressed but in-line),
        // anything else → send_document (preserves bytes). Voice files
        // would ideally use send_voice but the LLM may misclassify so we
        // err on the side of send_document for non-image media.
        let is_image = file.mime.starts_with("image/")
            && !matches!(file.mime, "image/svg+xml" | "image/heic" | "image/heif");

        if is_image {
            let mut req = self.bot.send_photo(chat, input);
            if let Some(c) = file.caption {
                if !c.is_empty() {
                    req = req.caption(c);
                }
            }
            req.await.map_err(|e| anyhow::anyhow!("telegram send_photo: {}", e))?;
        } else {
            let mut req = self.bot.send_document(chat, input);
            if let Some(c) = file.caption {
                if !c.is_empty() {
                    req = req.caption(c);
                }
            }
            req.await
                .map_err(|e| anyhow::anyhow!("telegram send_document: {}", e))?;
        }
        Ok(())
    }
}

#[async_trait]
impl OutboundChannel for TelegramOutbound {
    async fn send_to_principal(&self, principal_id: &str, content: &str) -> Result<()> {
        self.send_to_chat(principal_id, content).await
    }

    async fn send_to_room(&self, room_key: &str, content: &str) -> Result<()> {
        self.send_to_chat(room_key, content).await
    }

    async fn send_file_to_principal(
        &self,
        principal_id: &str,
        file: OutboundFile<'_>,
    ) -> Result<()> {
        self.send_file_to_chat(principal_id, file).await
    }

    async fn send_file_to_room(
        &self,
        room_key: &str,
        file: OutboundFile<'_>,
    ) -> Result<()> {
        self.send_file_to_chat(room_key, file).await
    }
}
