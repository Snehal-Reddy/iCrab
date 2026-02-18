//! iCrab— minimal personal AI assistant for iSH; Telegram-only.
//!
//! Single binary: runs Telegram poller + agent loop. Config: `~/.icrab/config.toml` or env.

use std::path::PathBuf;
use std::sync::Arc;

use icrab::agent;
use icrab::agent::subagent_manager::SubagentManager;
use icrab::config;
use icrab::llm::HttpProvider;
use icrab::telegram::{self, OutboundMsg};
use icrab::tools;
use icrab::tools::spawn::SpawnTool;
use icrab::tools::subagent::SubagentTool;

const SUBAGENT_MAX_ITERATIONS: u32 = 10;

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
        Ok(p) => Arc::new(p),
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
    let workspace = PathBuf::from(cfg.workspace_path());
    let restrict = cfg.restrict_to_workspace.unwrap_or(true);

    // Build subagent registry (core only — no spawn, no cron).
    let subagent_registry = Arc::new(tools::build_core_registry(&cfg));

    // SubagentManager: owns the subagent config and task map.
    let manager = Arc::new(SubagentManager::new(
        Arc::clone(&llm),
        subagent_registry,
        model.to_string(),
        workspace.clone(),
        restrict,
        SUBAGENT_MAX_ITERATIONS,
    ));

    // Main registry: core + spawn tool.
    let registry = tools::build_core_registry(&cfg);
    registry.register(SpawnTool::new(Arc::clone(&manager)));
    registry.register(SubagentTool::new(Arc::clone(&manager)));

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
