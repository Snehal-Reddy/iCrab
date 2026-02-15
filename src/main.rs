//! iCrab— minimal personal AI assistant for iSH; Telegram-only.
//!
//! Single binary: runs Telegram poller + agent loop. Config: `~/.icrab/config.toml` or env.

use icrab::config;
use icrab::telegram::{self, OutboundMsg};

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

    let (mut inbound_rx, outbound_tx) = telegram::spawn_telegram(&cfg);
    eprintln!("Telegram poller and sender started");

    // Agent loop: receive from Telegram, eventually run agent (context + LLM + tools), send reply.
    // For now placeholder: echo back until agent is implemented.
    while let Some(msg) = inbound_rx.recv().await {
        let reply = format!("Received (agent not yet connected): {}", msg.text);
        let _ = outbound_tx
            .send(OutboundMsg {
                chat_id: msg.chat_id,
                text: reply,
                channel: msg.channel,
            })
            .await;
    }
}
