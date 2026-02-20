//! Tool registry and implementations: file, web, message, cron, spawn; optional exec.

pub mod context;
pub mod cron;
pub mod file;
pub mod message;
pub mod registry;
pub mod result;
pub mod search;
pub mod spawn;
pub mod subagent;
pub mod web;

pub use context::ToolCtx;
pub use registry::{Tool, ToolRegistry, build_core_registry, build_default_registry, tool_to_def};
pub use result::ToolResult;
pub use search::SearchVaultTool;
