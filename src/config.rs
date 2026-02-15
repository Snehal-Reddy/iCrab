//! Config load, env overrides, validation.

use serde::Deserialize;

/// Root config: workspace, telegram, llm, optional tools.web, heartbeat, restrict_to_workspace.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub workspace: Option<String>,
    pub telegram: Option<TelegramConfig>,
    pub llm: Option<LlmConfig>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub restrict_to_workspace: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TelegramConfig {
    pub bot_token: Option<String>,
    pub allowed_user_ids: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LlmConfig {
    pub provider: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct HeartbeatConfig {
    pub interval_minutes: Option<u64>,
}
