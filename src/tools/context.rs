//! Execution context for tools: workspace, chat, outbound channel.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::telegram::OutboundMsg;

/// Context passed into each tool execution.
#[derive(Clone)]
pub struct ToolCtx {
    /// Workspace root (e.g. Obsidian vault path).
    pub workspace: PathBuf,
    /// If true, reject paths outside workspace (e.g. `..`).
    pub restrict_to_workspace: bool,
    /// Current chat ID for message tool (Telegram).
    pub chat_id: Option<i64>,
    /// Channel label (e.g. "telegram").
    pub channel: Option<String>,
    /// Send outbound messages (e.g. to Telegram). Used by message tool.
    pub outbound_tx: Option<Arc<mpsc::Sender<OutboundMsg>>>,
}
