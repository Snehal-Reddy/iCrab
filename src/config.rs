//! Config load, env overrides, validation.
//!
//! Single file: `~/.icrab/config.toml`. Override path with `ICRAB_CONFIG`.
//! Env overrides (optional): `TELEGRAM_BOT_TOKEN` or `ICRAB_TELEGRAM_BOT_TOKEN`,
//! `ICRAB_LLM_API_KEY`, `ICRAB_LLM_API_BASE`, `ICRAB_LLM_MODEL`, `ICRAB_WORKSPACE`,
//! `ICRAB_TOOLS_WEB_BRAVE_API_KEY`.

use std::path::PathBuf;

use serde::Deserialize;

/// Root config: workspace, telegram, llm, optional tools.web, heartbeat, restrict_to_workspace.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub workspace: Option<String>,
    pub telegram: Option<TelegramConfig>,
    pub llm: Option<LlmConfig>,
    #[serde(default)]
    pub tools: Option<ToolsConfig>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub restrict_to_workspace: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ToolsConfig {
    pub web: Option<WebConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WebConfig {
    pub brave_api_key: Option<String>,
    /// Max results for Brave/DDG search (1–10); default 5.
    pub brave_max_results: Option<u8>,
    /// Max chars for web_fetch body; default 50_000.
    pub web_fetch_max_chars: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TelegramConfig {
    pub bot_token: Option<String>,
    pub allowed_user_ids: Option<Vec<i64>>,
    /// Optional API base URL for testing or custom endpoints. Defaults to `https://api.telegram.org/bot{token}`.
    pub api_base: Option<String>,
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

/// Config load/validation errors.
#[derive(Debug, Clone)]
pub enum ConfigError {
    Io(String),
    Parse(String),
    Validation(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(s) => write!(f, "config io: {}", s),
            ConfigError::Parse(s) => write!(f, "config parse: {}", s),
            ConfigError::Validation(s) => write!(f, "config: {}", s),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Default config path: `$ICRAB_CONFIG` or `~/.icrab/config.toml`.
pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("ICRAB_CONFIG") {
        return PathBuf::from(p);
    }
    let mut dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    dir.push(".icrab");
    dir.push("config.toml");
    dir
}

/// Expand leading `~` to `$HOME`. No-op if no `~`.
fn expand_home(path: &str) -> String {
    let path = path.trim();
    if path.starts_with("~/") {
        if let Ok(h) = std::env::var("HOME") {
            return format!("{}{}", h, &path[1..]);
        }
    }
    if path == "~" {
        if let Ok(h) = std::env::var("HOME") {
            return h;
        }
    }
    path.to_string()
}

/// Load config from path: read TOML (if file exists), then apply env overrides.
pub fn load(path: &std::path::Path) -> Result<Config, ConfigError> {
    let mut cfg: Config = if path.exists() {
        let s = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        toml::from_str(&s).map_err(|e| ConfigError::Parse(e.to_string()))?
    } else {
        Config::default()
    };

    // Env overrides (secrets and key vars only)
    let bot_token =
        std::env::var("TELEGRAM_BOT_TOKEN").or_else(|_| std::env::var("ICRAB_TELEGRAM_BOT_TOKEN"));
    if let Ok(v) = bot_token {
        cfg.telegram
            .get_or_insert_with(TelegramConfig::default)
            .bot_token = Some(v);
    }
    if let Ok(v) = std::env::var("ICRAB_WORKSPACE") {
        cfg.workspace = Some(expand_home(&v));
    } else if let Some(ref w) = cfg.workspace {
        cfg.workspace = Some(expand_home(w));
    }
    if let Ok(v) = std::env::var("ICRAB_LLM_API_KEY") {
        cfg.llm.get_or_insert_with(LlmConfig::default).api_key = Some(v);
    }
    if let Ok(v) = std::env::var("ICRAB_LLM_API_BASE") {
        cfg.llm.get_or_insert_with(LlmConfig::default).api_base = Some(v);
    }
    if let Ok(v) = std::env::var("ICRAB_LLM_MODEL") {
        cfg.llm.get_or_insert_with(LlmConfig::default).model = Some(v);
    }
    if let Ok(v) = std::env::var("ICRAB_TOOLS_WEB_BRAVE_API_KEY") {
        let tools = cfg.tools.get_or_insert_with(ToolsConfig::default);
        let web = tools.web.get_or_insert_with(WebConfig::default);
        web.brave_api_key = Some(v);
    }

    cfg.validate()?;
    Ok(cfg)
}

impl Config {
    /// Validate required fields for running the gateway (Telegram + agent).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.workspace.as_deref().unwrap_or("").trim().is_empty() {
            return Err(ConfigError::Validation(
                "workspace is required (set in config or ICRAB_WORKSPACE)".to_string(),
            ));
        }
        if let Some(ref t) = self.telegram {
            if t.bot_token.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ConfigError::Validation(
                    "telegram.bot_token is required (or TELEGRAM_BOT_TOKEN)".to_string(),
                ));
            }
        } else {
            return Err(ConfigError::Validation(
                "telegram section is required".to_string(),
            ));
        }
        if let Some(ref l) = self.llm {
            if l.api_key.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ConfigError::Validation(
                    "llm.api_key is required (or ICRAB_LLM_API_KEY)".to_string(),
                ));
            }
            if l.model.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ConfigError::Validation(
                    "llm.model is required (or ICRAB_LLM_MODEL)".to_string(),
                ));
            }
        } else {
            return Err(ConfigError::Validation(
                "llm section is required".to_string(),
            ));
        }
        Ok(())
    }

    /// Resolved workspace path (after ~ expansion). Call after validate().
    pub fn workspace_path(&self) -> &str {
        self.workspace.as_deref().unwrap_or(".")
    }
}
