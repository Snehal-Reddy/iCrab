//! iCrab— minimal personal AI assistant for iSH; Telegram-only.
//!
//! Single binary: runs Telegram poller + agent loop. Config: `~/.icrab/config.toml` or env.

use std::path::PathBuf;
use std::sync::Arc;

use icrab::agent;
use icrab::config;
use icrab::llm::HttpProvider;
use icrab::telegram::{self, OutboundMsg};
use icrab::tools;

#[tokio::main]
async fn main() {
    eprintln!("icrab {}", env!("CARGO_PKG_VERSION"));
    let path = config::default_config_path();
    let cfg = match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!("workspace: {}", cfg.workspace_path());

    let llm = match HttpProvider::from_config(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("llm: {}", e);
            std::process::exit(1);
        }
    };
    let model = cfg
        .llm
        .as_ref()
        .and_then(|l| l.model.as_deref())
        .unwrap_or("google/gemini-3-flash-preview");
    let registry = tools::build_default_registry(&cfg);
    let workspace = PathBuf::from(cfg.workspace_path());
    let restrict = cfg.restrict_to_workspace.unwrap_or(true);

    let (mut inbound_rx, outbound_tx) = telegram::spawn_telegram(&cfg);
    eprintln!("Telegram poller and sender started");

    while let Some(msg) = inbound_rx.recv().await {
        let tool_ctx = tools::ToolCtx {
            workspace: workspace.clone(),
            restrict_to_workspace: restrict,
            chat_id: Some(msg.chat_id),
            channel: Some(msg.channel.clone()),
            outbound_tx: Some(Arc::new(outbound_tx.clone())),
        };
        let chat_id_str = msg.chat_id.to_string();
        let reply = match agent::process_message(
            &llm,
            &registry,
            &workspace,
            model,
            &chat_id_str,
            &msg.text,
            &tool_ctx,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("agent error: {}", e);
                format!("Error: {}.", e)
            }
        };
        let _ = outbound_tx
            .send(OutboundMsg {
                chat_id: msg.chat_id,
                text: reply,
                channel: msg.channel,
            })
            .await;
    }
}
