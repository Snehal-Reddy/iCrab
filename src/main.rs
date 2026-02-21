//! iCrab— minimal personal AI assistant for iSH; Telegram-only.
//!
//! Single binary: runs Telegram poller + agent loop. Config: `~/.icrab/config.toml` or env.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use icrab::agent;
use icrab::agent::subagent_manager::SubagentManager;
use icrab::config;
use icrab::cron_runner;
use icrab::heartbeat;
use icrab::llm::HttpProvider;
use icrab::memory::db::BrainDb;
use icrab::memory::indexer::VaultIndexer;
use icrab::sync;
use icrab::tools::{GitSyncTool, GrepDirTool, SearchChatTool, SearchVaultTool};
use icrab::telegram::{self, OutboundMsg};
use icrab::tools;
use icrab::tools::cron::{CronStore, CronTool};
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
    let timezone = cfg
        .timezone
        .as_deref()
        .unwrap_or("Europe/London")
        .to_string();

    // Open the SQLite brain DB once at startup; shared across all message processing.
    let db = match BrainDb::open(&workspace) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("brain db: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!("brain db opened: {}", icrab::workspace::brain_db_path(&workspace).display());

    // Kick off the vault indexer in a background task so startup isn't blocked.
    // The indexer walks the workspace and upserts any new/modified .md files
    // into vault_index (FTS5 stays in sync via triggers).  Errors are logged
    // but never fatal.
    {
        let indexer = VaultIndexer::new(Arc::clone(&db));
        let ws_clone = workspace.clone();
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || indexer.scan(&ws_clone)).await {
                Ok(Ok(stats)) => eprintln!("vault index: {stats}"),
                Ok(Err(e)) => eprintln!("vault index warning: {e}"),
                Err(e) => eprintln!("vault index task error: {e}"),
            }
        });
    }

    // Background git pull + re-index loop (every 15 min).
    sync::spawn_git_pull_loop(
        workspace.clone(),
        Arc::clone(&db),
        sync::DEFAULT_PULL_INTERVAL_SECS,
    );
    eprintln!(
        "background git pull loop started (interval: {}h)",
        sync::DEFAULT_PULL_INTERVAL_SECS / 3600
    );

    // Build subagent registry (core + search tools — no spawn, no cron).
    let subagent_registry = Arc::new({
        let reg = tools::build_core_registry(&cfg);
        reg.register(SearchVaultTool::new(Arc::clone(&db)));
        reg.register(SearchChatTool::new(Arc::clone(&db)));
        reg.register(GrepDirTool);
        reg
    });

    // SubagentManager: owns the subagent config and task map.
    let manager = Arc::new(SubagentManager::new(
        Arc::clone(&llm),
        subagent_registry,
        model.to_string(),
        workspace.clone(),
        restrict,
        SUBAGENT_MAX_ITERATIONS,
    ));

    // Main registry: core + search + git + grep + spawn + cron.
    let registry = tools::build_core_registry(&cfg);
    registry.register(SearchVaultTool::new(Arc::clone(&db)));
    registry.register(SearchChatTool::new(Arc::clone(&db)));
    registry.register(GrepDirTool);
    registry.register(GitSyncTool);
    registry.register(SpawnTool::new(Arc::clone(&manager)));
    registry.register(SubagentTool::new(Arc::clone(&manager)));

    let (inbound_tx, mut inbound_rx) = mpsc::channel(64);
    let outbound_tx = telegram::spawn_telegram(&cfg, inbound_tx.clone());
    eprintln!("Telegram poller and sender started");

    let cron_store = Arc::new(
        CronStore::load(&workspace).unwrap_or_else(|e| {
            eprintln!("cron store: {}", e);
            CronStore::empty(&workspace)
        }),
    );
    cron_runner::spawn_cron_runner(
        Arc::clone(&cron_store),
        inbound_tx.clone(),
        outbound_tx.clone(),
        60,
    );
    registry.register(CronTool::new(Arc::clone(&cron_store)));

    // Track the last Telegram/cron chat_id so heartbeat replies go to the right chat.
    let last_chat_id: Arc<AtomicI64> = Arc::new(AtomicI64::new(0));

    // Spawn heartbeat if configured with interval_minutes >= 1.
    let heartbeat_interval = cfg
        .heartbeat
        .as_ref()
        .and_then(|h| h.interval_minutes)
        .unwrap_or(0);
    if heartbeat_interval >= 1 {
        heartbeat::spawn_heartbeat_runner(
            workspace.clone(),
            heartbeat_interval,
            inbound_tx.clone(),
            Arc::clone(&last_chat_id),
        );
        eprintln!("heartbeat runner started (interval: {} min)", heartbeat_interval);
    }

    drop(inbound_tx);

    while let Some(msg) = inbound_rx.recv().await {
        // Update last_chat_id for non-heartbeat sources so replies go to the right place.
        if msg.channel != "heartbeat" {
            last_chat_id.store(msg.chat_id, Ordering::Relaxed);
        }

        let tool_ctx = tools::ToolCtx {
            workspace: workspace.clone(),
            restrict_to_workspace: restrict,
            chat_id: Some(msg.chat_id),
            channel: Some(msg.channel.clone()),
            outbound_tx: Some(Arc::new(outbound_tx.clone())),
        };
        let chat_id_str = msg.chat_id.to_string();

        let reply = if msg.channel == "heartbeat" {
            match agent::process_heartbeat_message(
                &llm,
                &registry,
                &workspace,
                model,
                &timezone,
                &chat_id_str,
                &msg.text,
                &tool_ctx,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("heartbeat agent error: {}", e);
                    format!("Error: {}.", e)
                }
            }
        } else {
            match agent::process_message(
                &llm,
                &registry,
                &workspace,
                model,
                &timezone,
                &chat_id_str,
                &msg.text,
                &tool_ctx,
                &db,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("agent error: {}", e);
                    format!("Error: {}.", e)
                }
            }
        };

        // Heartbeat with no known chat (chat_id == 0): no user has messaged yet, drop reply.
        if msg.channel == "heartbeat" && msg.chat_id == 0 {
            continue;
        }

        let _ = outbound_tx
            .send(OutboundMsg {
                chat_id: msg.chat_id,
                text: reply,
                channel: msg.channel,
            })
            .await;
    }
}
