//! spawn tool: run a subagent in the background; returns immediately with task ID.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::subagent_manager::SubagentManager;
use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

/// Spawn tool: starts a subagent task in the background.
pub struct SpawnTool {
    manager: Arc<SubagentManager>,
}

impl SpawnTool {
    #[inline]
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
    }
}

impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "Run a subagent in the background to complete a task independently. It reports results via message."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the subagent to complete"
                },
                "label": {
                    "type": "string",
                    "description": "Optional short label for this subagent task"
                }
            },
            "required": ["task"]
        })
    }

    fn execute<'a>(&'a self, ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let manager = self.manager.clone();
        let args = args.clone();
        let ctx = ctx.clone();

        Box::pin(async move {
            let task = match args.get("task").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => return ToolResult::error("missing or empty 'task' argument"),
            };
            let label = args.get("label").and_then(Value::as_str).map(String::from);

            let Some(chat_id) = ctx.chat_id else {
                return ToolResult::error("spawn unavailable: no chat_id");
            };
            let Some(ref outbound_tx) = ctx.outbound_tx else {
                return ToolResult::error("spawn unavailable: no outbound channel");
            };
            let channel = ctx
                .channel
                .clone()
                .unwrap_or_else(|| "telegram".to_string());

            let task_id = manager.spawn(
                task,
                label.clone(),
                chat_id,
                Arc::clone(outbound_tx),
                channel,
            );

            let display_label = label.as_deref().unwrap_or("task");
            ToolResult::async_(format!(
                "Subagent '{}' started (id: {}). It will report back when done.",
                display_label, task_id
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_tool_name_and_description() {
        let mgr = Arc::new(test_manager());
        let tool = SpawnTool::new(mgr);
        assert_eq!(tool.name(), "spawn");
        assert!(tool.description().contains("subagent"));
        assert!(tool.description().contains("background"));
    }

    #[tokio::test]
    async fn execute_missing_task_returns_error() {
        let mgr = Arc::new(test_manager());
        let tool = SpawnTool::new(mgr);
        let ctx = test_ctx(true);
        let res = tool.execute(&ctx, &serde_json::json!({})).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("task"));
    }

    #[tokio::test]
    async fn execute_missing_chat_id_returns_error() {
        let mgr = Arc::new(test_manager());
        let tool = SpawnTool::new(mgr);
        let mut ctx = test_ctx(true);
        ctx.chat_id = None;
        let res = tool
            .execute(&ctx, &serde_json::json!({"task": "do something"}))
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn execute_missing_outbound_returns_error() {
        let mgr = Arc::new(test_manager());
        let tool = SpawnTool::new(mgr);
        let mut ctx = test_ctx(true);
        ctx.outbound_tx = None;
        let res = tool
            .execute(&ctx, &serde_json::json!({"task": "do something"}))
            .await;
        assert!(res.is_error);
    }

    // -- helpers --

    fn test_manager() -> SubagentManager {
        let cfg = crate::config::Config {
            workspace: Some("/tmp".into()),
            restrict_to_workspace: Some(true),
            telegram: None,
            llm: Some(crate::config::LlmConfig {
                provider: None,
                api_base: Some("http://localhost:1".into()),
                api_key: Some("test".into()),
                model: Some("test".into()),
            }),
            tools: None,
            heartbeat: None,
            timezone: None,
        };
        let llm = crate::llm::HttpProvider::from_config(&cfg).expect("stub");
        SubagentManager::new(
            Arc::new(llm),
            Arc::new(crate::tools::registry::ToolRegistry::new()),
            "test".into(),
            std::path::PathBuf::from("/tmp"),
            true,
            5,
        )
    }

    fn test_ctx(full: bool) -> ToolCtx {
        if full {
            let (tx, _rx) = tokio::sync::mpsc::channel(4);
            ToolCtx {
                workspace: std::path::PathBuf::from("/tmp"),
                restrict_to_workspace: true,
                chat_id: Some(123),
                channel: Some("telegram".into()),
                outbound_tx: Some(Arc::new(tx)),
            }
        } else {
            ToolCtx {
                workspace: std::path::PathBuf::from("/tmp"),
                restrict_to_workspace: true,
                chat_id: None,
                channel: None,
                outbound_tx: None,
            }
        }
    }
}
