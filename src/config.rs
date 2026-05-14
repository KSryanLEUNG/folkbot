//! Config loader. Reads a TOML file and resolves the API key from an env var.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use crate::llm::{openai::OpenAiCompatProvider, LlmProvider};
use crate::mcp::McpServerConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub llm: LlmConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    /// Env var holding the Telegram Bot API token (`TELEGRAM_BOT_TOKEN` recommended).
    pub bot_token_env: Option<String>,
    /// Or hardcoded (not recommended).
    pub bot_token: Option<String>,
    /// Telegram user IDs allowed to DM the bot. Empty = reject everyone (safe
    /// default — explicit allowlist is required to use Telegram).
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl TelegramConfig {
    pub fn resolve_token(&self) -> Result<String> {
        if let Some(env_var) = &self.bot_token_env {
            return std::env::var(env_var)
                .with_context(|| format!("env var {} not set", env_var));
        }
        self.bot_token
            .clone()
            .ok_or_else(|| anyhow!("channels.telegram: bot_token_env or bot_token required"))
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
}

impl LlmConfig {
    pub fn resolve_api_key(&self) -> Result<String> {
        if let Some(env_var) = &self.api_key_env {
            return std::env::var(env_var)
                .with_context(|| format!("env var {} not set", env_var));
        }
        self.api_key
            .clone()
            .ok_or_else(|| anyhow!("config: api_key_env or api_key required"))
    }

    pub fn build_provider(&self) -> Result<Arc<dyn LlmProvider>> {
        let key = self.resolve_api_key()?;
        match self.provider.as_str() {
            "openai" => Ok(Arc::new(OpenAiCompatProvider::new(
                self.base_url.clone(),
                key,
                self.model.clone(),
            ))),
            "anthropic" => Err(anyhow!("provider 'anthropic' not implemented yet")),
            "ollama" => Err(anyhow!(
                "provider 'ollama' not implemented yet \
                 (use 'openai' with base_url=http://localhost:11434/v1)"
            )),
            other => Err(anyhow!("unknown provider: {}", other)),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct AgentConfig {
    pub system_prompt: Option<String>,
    /// Optional separate model used only for summarization (cheap model
    /// recommended). Falls back to `[llm]` when omitted.
    pub summarizer: Option<LlmConfig>,
    #[serde(default)]
    pub compressor: CompressorConfig,
    /// Local timezone offset in seconds. Defaults to +28800 (Asia/Hong_Kong)
    /// when omitted, matching v1.2 behaviour. Used by `fmt_ts` and the
    /// daily-summary boundary calculation. Applied at startup; restart to
    /// change.
    pub timezone_offset_secs: Option<i64>,
}

impl AgentConfig {
    pub fn tz_offset(&self) -> i64 {
        self.timezone_offset_secs
            .unwrap_or(crate::util::DEFAULT_TZ_OFFSET_SECS)
    }
}

#[derive(Debug, Deserialize)]
pub struct CompressorConfig {
    /// How often the background tokio task wakes up and runs the daily +
    /// cascade compressor. `0` disables the task entirely.
    /// Default: 3600 (1 hour).
    #[serde(default = "default_compressor_interval")]
    pub interval_seconds: u64,
}

impl Default for CompressorConfig {
    fn default() -> Self {
        Self { interval_seconds: default_compressor_interval() }
    }
}

fn default_compressor_interval() -> u64 {
    1800
}

#[derive(Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default, rename = "servers")]
    pub servers: Vec<McpServerConfig>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }

    /// Build the (main_llm, summarizer_llm) pair. `summarizer_llm` falls
    /// back to `main_llm` if `[agent.summarizer]` is not in config.
    pub fn build_llms(&self) -> Result<(Arc<dyn LlmProvider>, Arc<dyn LlmProvider>)> {
        let main = self.llm.build_provider()?;
        let summarizer = match &self.agent.summarizer {
            Some(s) => s.build_provider()?,
            None => main.clone(),
        };
        Ok((main, summarizer))
    }
}
