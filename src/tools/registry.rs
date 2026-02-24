//! Register tools by name; name, description, JSON schema, execute(ctx, args) -> ToolResult.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::config::Config;
use crate::llm::ToolDef;
use crate::tools::context::ToolCtx;
use crate::tools::file::{AppendFile, EditFile, ListDir, ReadFile, WriteFile};
use crate::tools::result::ToolResult;
use crate::tools::web::{WebFetchTool, WebSearchProvider, WebSearchTool, web_client};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A single tool: name, description, JSON schema for args, and execute.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execute<'a>(&'a self, ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult>;
}

/// Convert a tool to LLM provider tool definition.
#[inline]
pub fn tool_to_def(tool: &dyn Tool) -> ToolDef {
    ToolDef::function(
        tool.name().to_string(),
        tool.description().to_string(),
        tool.parameters(),
    )
}

/// Registry of tools by name. Thread-safe; cheap to clone (Arc inside).
#[derive(Default)]
pub struct ToolRegistry {
    inner: RwLock<HashMap<String, Arc<dyn Tool + Send + Sync>>>,
}

impl ToolRegistry {
    #[inline]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Register a tool by its name. Overwrites if name already exists.
    pub fn register<T: Tool + Send + Sync + 'static>(&self, tool: T) {
        let name = tool.name().to_string();
        self.inner
            .write()
            .expect("registry lock")
            .insert(name, Arc::new(tool));
    }

    /// Execute tool by name. Returns error result if not found.
    pub async fn execute(&self, ctx: &ToolCtx, name: &str, args: &Value) -> ToolResult {
        let tool = {
            let guard = self.inner.read().expect("registry lock");
            guard.get(name).cloned()
        };

        if let Some(tool) = tool {
            tool.execute(ctx, args).await
        } else {
            ToolResult::error(format!("tool '{name}' not found"))
        }
    }

    /// All tool definitions for the LLM.
    pub fn to_tool_defs(&self) -> Vec<ToolDef> {
        let guard = self.inner.read().expect("registry lock");
        guard.values().map(|t| tool_to_def(t.as_ref())).collect()
    }

    /// Sorted list of tool names.
    pub fn list(&self) -> Vec<String> {
        let guard = self.inner.read().expect("registry lock");
        let mut names: Vec<String> = guard.keys().cloned().collect();
        names.sort();
        names
    }

    /// Short summaries: "name - description" per tool, sorted by name.
    pub fn summaries(&self) -> Vec<String> {
        let guard = self.inner.read().expect("registry lock");
        let mut pairs: Vec<(String, String)> = guard
            .iter()
            .map(|(n, t)| (n.clone(), t.description().to_string()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
            .into_iter()
            .map(|(n, d)| format!("{n} - {d}"))
            .collect()
    }
}

const DEFAULT_BRAVE_MAX_RESULTS: u8 = 5;
const DEFAULT_WEB_FETCH_MAX_CHARS: u32 = 50_000;

/// Build the core registry (file + web).  Used as the base for both the
/// main-agent registry and the subagent registry.
///
/// `MessageTool` is intentionally NOT included here. It is only added to
/// subagent registries, where background tasks need to push results to the
/// user. In the main agent the reply is returned as text content; offering
/// `message` there causes the LLM to send duplicate replies.
pub fn build_core_registry(config: &Config) -> ToolRegistry {
    let reg = ToolRegistry::new();
    reg.register(ReadFile);
    reg.register(WriteFile);
    reg.register(ListDir);
    reg.register(EditFile);
    reg.register(AppendFile);

    let web_cfg = config.tools.as_ref().and_then(|t| t.web.as_ref());
    let brave_max_results = web_cfg
        .and_then(|w| w.brave_max_results)
        .unwrap_or(DEFAULT_BRAVE_MAX_RESULTS)
        .clamp(1, 10);
    let fetch_max_chars = web_cfg
        .and_then(|w| w.web_fetch_max_chars)
        .unwrap_or(DEFAULT_WEB_FETCH_MAX_CHARS);

    if let Ok(client) = web_client() {
        let provider = web_cfg
            .and_then(|w| w.brave_api_key.as_deref())
            .filter(|k| !k.is_empty())
            .map(|api_key| WebSearchProvider::Brave {
                api_key: api_key.to_string(),
                max_results: brave_max_results,
            })
            .unwrap_or(WebSearchProvider::DuckDuckGo {
                max_results: brave_max_results,
            });
        reg.register(WebSearchTool::new(provider, client.clone()));
        reg.register(WebFetchTool::new(client, fetch_max_chars));
    }

    reg
}

/// Build the default (main-agent) registry: core tools only.
/// Caller adds spawn (and later cron) after constructing SubagentManager.
#[inline]
pub fn build_default_registry(config: &Config) -> ToolRegistry {
    build_core_registry(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::file::ReadFile;

    #[tokio::test]
    async fn registry_register_execute_to_tool_defs() {
        let reg = ToolRegistry::new();
        reg.register(ReadFile);
        assert!(reg.list().contains(&"read_file".to_string()));
        let defs = reg.to_tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "read_file");
        let summaries = reg.summaries();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].starts_with("read_file - "));

        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            restrict_to_workspace: true,
            chat_id: None,
            channel: None,
            outbound_tx: None,
        };
        let args = serde_json::json!({ "path": "." });
        let res = reg.execute(&ctx, "read_file", &args).await;
        assert!(res.is_error); // . is a dir, not a file
        let res = reg.execute(&ctx, "unknown", &serde_json::json!({})).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("not found"));
    }
}
