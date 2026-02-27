//! subagent tool: run a subagent task synchronously and block until completion.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::run_agent_loop;
use crate::agent::subagent_manager::SubagentManager;
use crate::llm::{Message, Role};
use crate::skills;
use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

/// Subagent tool: runs a subagent task synchronously.
pub struct SubagentTool {
    manager: Arc<SubagentManager>,
}

impl SubagentTool {
    #[inline]
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
    }
}

impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Execute a subagent task synchronously and return the result. Use for delegating specific tasks to an independent agent instance. Returns execution summary to user and full details to LLM."
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

        // Clone context components we need for the subagent
        let chat_id = ctx.chat_id;
        let outbound_tx = ctx.outbound_tx.clone();
        let channel = ctx
            .channel
            .clone()
            .unwrap_or_else(|| "telegram".to_string());
        // Share the parent's delivered flag so the message tool inside the
        // subagent propagates the "already sent" state back to main.rs.
        let delivered = ctx.delivered.clone();

        // We construct a new ToolCtx for the subagent that shares the outbound
        // capabilities of the parent and the delivered flag.
        let sub_ctx = ToolCtx {
            workspace: manager.workspace().clone(),
            restrict_to_workspace: manager.restrict_to_workspace(),
            chat_id,
            channel: Some(channel),
            outbound_tx,
            delivered,
        };

        Box::pin(async move {
            let task = match args.get("task").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => return ToolResult::error("missing or empty 'task' argument"),
            };
            let label = args.get("label").and_then(Value::as_str).map(String::from);

            // --- Build system prompt (logic duplicated from agent::run_subagent) ---
            let mut system = String::from(
                "You are a subagent. Complete the given task independently and report the result.

                 You have access to tools - use them as needed to complete your task.

                 After completing the task, provide a clear summary of what was done.

                 Send your result to the user with the message tool.
",
            );

            // Skills
            match skills::build_skills_summary(manager.workspace()) {
                Ok(ref s) if !s.is_empty() => {
                    system.push_str(
                        "
--- Skills ---
",
                    );
                    system.push_str(s);
                    system.push('\n');
                }
                Err(e) => {
                    // Log error but continue? The original run_subagent prints to stderr.
                    // We can include it in the error result or just log it.
                    // For now, let's just log to stderr to match run_subagent behavior.
                    eprintln!("subagent tool: skills error: {}", e);
                }
                _ => {}
            }

            // Tool summaries
            let summaries = manager.registry().summaries();
            if !summaries.is_empty() {
                system.push_str(
                    "
--- Tools ---
",
                );
                for line in &summaries {
                    system.push_str(line);
                    system.push('\n');
                }
            }

            let messages = vec![
                Message {
                    role: Role::System,
                    content: system,
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message {
                    role: Role::User,
                    content: task,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ];

            // --- Run Agent Loop Synchronously ---
            match run_agent_loop(
                manager.llm(),
                manager.registry(),
                messages,
                &sub_ctx,
                manager.model(),
                manager.max_iterations(),
            )
            .await
            {
                Ok(content) => {
                    let display_label = label.as_deref().unwrap_or("task");

                    // The subagent is expected to have delivered its result to the user
                    // via the message tool (setting the delivered flag).  We do NOT set
                    // for_user here to avoid Path-C duplication.  The LLM receives the
                    // full result and is instructed not to repeat it.
                    let for_llm = format!(
                        "Subagent '{}' completed. Result already sent to user via message tool. Do not repeat it.\nResult:\n{}",
                        display_label, content
                    );

                    ToolResult::ok(for_llm)
                }
                Err(e) => ToolResult::error(format!("Subagent execution failed: {}", e)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_tool_metadata() {
        let mgr = Arc::new(test_manager());
        let tool = SubagentTool::new(mgr);
        assert_eq!(tool.name(), "subagent");
        assert!(tool.description().contains("synchronously"));
    }

    #[tokio::test]
    async fn execute_missing_task_returns_error() {
        let mgr = Arc::new(test_manager());
        let tool = SubagentTool::new(mgr);
        let ctx = test_ctx();
        let res = tool.execute(&ctx, &serde_json::json!({})).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("task"));
    }

    // Helpers
    fn test_manager() -> SubagentManager {
        // Dummy config for test construction
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
        // This might fail if Config::validate() checks paths, but here we just need types.
        // Actually HttpProvider::from_config might check stuff.
        // We can reuse the stub_provider pattern from subagent_manager tests if needed.
        // But let's try this.
        let llm = crate::llm::HttpProvider::from_config(&cfg).unwrap();
        SubagentManager::new(
            Arc::new(llm),
            Arc::new(crate::tools::registry::ToolRegistry::new()),
            "test".into(),
            std::path::PathBuf::from("/tmp"),
            true,
            5,
        )
    }

    fn test_ctx() -> ToolCtx {
        ToolCtx {
            workspace: std::path::PathBuf::from("/tmp"),
            restrict_to_workspace: true,
            chat_id: Some(123),
            channel: Some("telegram".into()),
            outbound_tx: None,
            delivered: Default::default(),
        }
    }
}
