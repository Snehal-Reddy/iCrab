//! Telegram poller: getUpdates (long poll), allow-list, sendMessage; glue to agent in/out.
//!
//! Single long-poll input, replies via sendMessage. No webhooks, no SDK.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{Config, TelegramConfig};

// --- Channel types (bounded mpsc, cap 32–64) ---

/// One user message from Telegram; agent receives these.
#[derive(Debug, Clone)]
pub struct InboundMsg {
    pub chat_id: i64,
    pub user_id: i64,
    pub text: String,
    /// Optional channel label for multi-channel or logging (e.g. "telegram").
    #[allow(dead_code)]
    pub channel: String,
}

/// One reply to send to Telegram; agent/tools send these.
#[derive(Debug, Clone)]
pub struct OutboundMsg {
    pub chat_id: i64,
    pub text: String,
    #[allow(dead_code)]
    pub channel: String,
}

/// Errors from Telegram API or HTTP; poll loop retries without advancing offset on transient failures.
#[derive(Debug)]
pub enum TelegramError {
    Http(String),
    Parse(String),
    Api { code: i64, description: String },
}

impl std::fmt::Display for TelegramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelegramError::Http(s) => write!(f, "telegram http: {}", s),
            TelegramError::Parse(s) => write!(f, "telegram parse: {}", s),
            TelegramError::Api { code, description } => {
                write!(f, "telegram api {}: {}", code, description)
            }
        }
    }
}

impl std::error::Error for TelegramError {}

/// Format a reqwest/HTTP error and its source chain for logging (surfaces TLS, DNS, etc.).
fn format_error_chain(e: &impl std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        s.push_str(" | ");
        s.push_str(&inner.to_string());
        src = inner.source();
    }
    s
}

// --- Minimal Telegram API structs ---

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    from: Option<From>,
    #[serde(default)]
    chat: Option<Chat>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct From {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Debug, Serialize)]
struct SendMessageBody {
    chat_id: i64,
    text: String,
}

#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    #[serde(default)]
    #[allow(dead_code)]
    ok: bool,
    #[serde(default)]
    error_code: i64,
    #[serde(default)]
    description: String,
}

const CHANNEL_CAP: usize = 64;
const GET_UPDATES_TIMEOUT_SECS: u64 = 25;
const HTTP_TIMEOUT_SECS: u64 = 30;
const TELEGRAM_MAX_MESSAGE_LEN: usize = 4096;
const TRUNCATE_TO: usize = 4090;

/// Shared Telegram API client: getUpdates and sendMessage.
struct TelegramClient {
    client: reqwest::Client,
    base_url: String,
}

impl TelegramClient {
    fn new(bot_token: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .expect("reqwest client");
        let base_url = format!("https://api.telegram.org/bot{}", bot_token);
        Self { client, base_url }
    }

    async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<(i64, i64, i64, String)>, TelegramError> {
        let url = format!(
            "{}/getUpdates?offset={}&timeout={}",
            self.base_url, offset, timeout_secs
        );
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| TelegramError::Http(format_error_chain(&e)))?;
        let status = res.status();
        let body = res
            .text()
            .await
            .map_err(|e| TelegramError::Http(format_error_chain(&e)))?;

        if !status.is_success() {
            if let Ok(api_err) = serde_json::from_str::<ApiErrorResponse>(&body) {
                return Err(TelegramError::Api {
                    code: api_err.error_code,
                    description: api_err.description,
                });
            }
            return Err(TelegramError::Http(format!("{} {}", status, body)));
        }

        let parsed: GetUpdatesResponse =
            serde_json::from_str(&body).map_err(|e| TelegramError::Parse(e.to_string()))?;
        if !parsed.ok {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for update in parsed.result {
            if let Some(msg) = update.message {
                let text = match msg.text {
                    Some(t) if !t.is_empty() => t,
                    _ => continue,
                };
                let from_id = msg.from.as_ref().map(|f| f.id);
                let chat_id = msg.chat.as_ref().map(|c| c.id);
                match (from_id, chat_id) {
                    (Some(uid), Some(cid)) => out.push((update.update_id, cid, uid, text)),
                    _ => continue,
                }
            }
        }
        Ok(out)
    }

    async fn send_message(&self, chat_id: i64, text: String) -> Result<(), TelegramError> {
        let url = format!("{}/sendMessage", self.base_url);
        let mut text = text;
        let mut retried = false;
        loop {
            let body = SendMessageBody {
                chat_id,
                text: text.clone(),
            };
            let res = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| TelegramError::Http(format_error_chain(&e)))?;
            let status = res.status();
            let body_str = res
                .text()
                .await
                .map_err(|e| TelegramError::Http(format_error_chain(&e)))?;

            if status.is_success() {
                return Ok(());
            }

            if status.as_u16() == 400 && !retried {
                if let Ok(api_err) = serde_json::from_str::<ApiErrorResponse>(&body_str) {
                    if text.len() > TELEGRAM_MAX_MESSAGE_LEN
                        && api_err.description.contains("message is too long")
                    {
                        text = format!("{}...", text.chars().take(TRUNCATE_TO).collect::<String>());
                        retried = true;
                        continue;
                    }
                }
            }
            if status.as_u16() == 400 {
                if let Ok(api_err) = serde_json::from_str::<ApiErrorResponse>(&body_str) {
                    return Err(TelegramError::Api {
                        code: api_err.error_code,
                        description: api_err.description,
                    });
                }
            }
            return Err(TelegramError::Http(format!("{} {}", status, body_str)));
        }
    }
}

/// True if user is allowed: empty/None list = allow all (document: setting IDs recommended for security).
fn is_allowed(cfg: &TelegramConfig, user_id: i64) -> bool {
    match &cfg.allowed_user_ids {
        None => true,
        Some(ids) if ids.is_empty() => true,
        Some(ids) => ids.contains(&user_id),
    }
}

/// Poll loop: long poll getUpdates, filter by allow-list, push InboundMsg to channel.
async fn poll_loop(
    client: TelegramClient,
    bot_token: String,
    allowed_user_ids: Option<Vec<i64>>,
    inbound_tx: mpsc::Sender<InboundMsg>,
) {
    let cfg = TelegramConfig {
        bot_token: Some(bot_token),
        allowed_user_ids,
    };
    let mut offset: i64 = 0;
    let mut backoff_secs = 1u64;

    loop {
        match client.get_updates(offset, GET_UPDATES_TIMEOUT_SECS).await {
            Ok(updates) => {
                backoff_secs = 1;
                let mut max_update_id = offset;
                for (update_id, chat_id, user_id, text) in updates {
                    max_update_id = max_update_id.max(update_id);
                    if !is_allowed(&cfg, user_id) {
                        continue;
                    }
                    let msg = InboundMsg {
                        chat_id,
                        user_id,
                        text,
                        channel: "telegram".to_string(),
                    };
                    if inbound_tx.send(msg).await.is_err() {
                        return;
                    }
                }
                offset = max_update_id + 1;
            }
            Err(e) => {
                eprintln!(
                    "telegram getUpdates error: {} (backoff {}s)",
                    e, backoff_secs
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
            }
        }
    }
}

/// Send loop: receive OutboundMsg from channel, call send_message; truncate and retry once on 400 if len > 4096.
async fn send_loop(client: TelegramClient, mut outbound_rx: mpsc::Receiver<OutboundMsg>) {
    while let Some(msg) = outbound_rx.recv().await {
        if let Err(e) = client.send_message(msg.chat_id, msg.text).await {
            eprintln!("telegram sendMessage error: {}", e);
        }
    }
}

/// Spawns the Telegram poll task and send task; returns channels for main/agent.
///
/// Main holds `inbound_rx` and `outbound_tx`. Poll loop pushes allowed user messages to inbound;
/// main/agent sends replies via outbound_tx. Shutdown in v1: process kill; later add cancel token.
pub fn spawn_telegram(config: &Config) -> (mpsc::Receiver<InboundMsg>, mpsc::Sender<OutboundMsg>) {
    let telegram = config.telegram.as_ref().expect("config validated");
    let bot_token = telegram.bot_token.clone().expect("config validated");
    let allowed_user_ids = telegram.allowed_user_ids.clone();

    let client = TelegramClient::new(&bot_token);
    let (inbound_tx, inbound_rx) = mpsc::channel(CHANNEL_CAP);
    let (outbound_tx, outbound_rx) = mpsc::channel(CHANNEL_CAP);

    let poll_client = TelegramClient {
        client: client.client.clone(),
        base_url: client.base_url.clone(),
    };
    tokio::spawn(
        async move { poll_loop(poll_client, bot_token, allowed_user_ids, inbound_tx).await },
    );

    tokio::spawn(async move {
        send_loop(client, outbound_rx).await;
    });

    (inbound_rx, outbound_tx)
}
