//! read_file, write_file, list_dir, edit_file, append_file — workspace-only, path restriction.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::tools::context::ToolCtx;
use crate::tools::registry::Tool;
use crate::tools::result::ToolResult;

/// Resolve path relative to workspace; reject `..` and paths outside workspace when restrict is true.
/// Does not require the path to exist (for write/append).
pub fn resolve_path(
    path: &str,
    workspace: &Path,
    restrict: bool,
) -> Result<PathBuf, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("path is empty".into());
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|e| e.to_string())
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut current = workspace.clone();
    for comp in Path::new(path).components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                return Err("absolute path not allowed when restricted".into());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !current.pop() {
                    return Err("path escapes workspace".into());
                }
                if !current.starts_with(&workspace) {
                    return Err("path escapes workspace".into());
                }
            }
            Component::Normal(p) => current.push(p),
        }
    }
    if restrict && !current.starts_with(&workspace) {
        return Err("path escapes workspace".into());
    }
    Ok(current)
}

fn get_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("missing or invalid '{key}'"))
}

fn get_optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

/// read_file tool.
pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file in the workspace. Path is relative to workspace."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to workspace" }
            },
            "required": ["path"]
        })
    }

    fn execute(&self, ctx: &ToolCtx, args: &Value) -> ToolResult {
        let path = match get_string(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let resolved = match resolve_path(&path, &ctx.workspace, ctx.restrict_to_workspace) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        match fs::read_to_string(&resolved) {
            Ok(content) => ToolResult::ok(content),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

/// write_file tool.
pub struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Overwrite a file in the workspace with the given content. Path is relative to workspace."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to workspace" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
        })
    }

    fn execute(&self, ctx: &ToolCtx, args: &Value) -> ToolResult {
        let path = match get_string(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let content = match get_string(args, "content") {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e),
        };
        let resolved = match resolve_path(&path, &ctx.workspace, ctx.restrict_to_workspace) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        if let Some(parent) = resolved.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return ToolResult::error(e.to_string());
            }
        }
        match fs::write(&resolved, content) {
            Ok(()) => ToolResult::ok("written"),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

/// list_dir tool.
pub struct ListDir;

impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List directory contents in the workspace. Path optional (default workspace root)."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to workspace (optional)" }
            }
        })
    }

    fn execute(&self, ctx: &ToolCtx, args: &Value) -> ToolResult {
        let path = get_optional_string(args, "path").unwrap_or_else(|| ".".to_string());
        let resolved = match resolve_path(&path, &ctx.workspace, ctx.restrict_to_workspace) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        match fs::read_dir(&resolved) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect();
                names.sort();
                ToolResult::ok(names.join("\n"))
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

/// edit_file tool (replace old_text with new_text in file).
pub struct EditFile;

impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace old_text with new_text in a file. Path relative to workspace."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to workspace" },
                "old_text": { "type": "string", "description": "Exact text to replace" },
                "new_text": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    fn execute(&self, ctx: &ToolCtx, args: &Value) -> ToolResult {
        let path = match get_string(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let old_text = match get_string(args, "old_text") {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e),
        };
        let new_text = match get_string(args, "new_text") {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e),
        };
        let resolved = match resolve_path(&path, &ctx.workspace, ctx.restrict_to_workspace) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let content = match fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let new_content = content.replacen(&old_text, &new_text, 1);
        if new_content == content {
            return ToolResult::error("old_text not found in file");
        }
        match fs::write(&resolved, new_content) {
            Ok(()) => ToolResult::ok("edited"),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

/// append_file tool.
pub struct AppendFile;

impl Tool for AppendFile {
    fn name(&self) -> &str {
        "append_file"
    }

    fn description(&self) -> &str {
        "Append content to a file in the workspace. Creates file if missing."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to workspace" },
                "content": { "type": "string", "description": "Content to append" }
            },
            "required": ["path", "content"]
        })
    }

    fn execute(&self, ctx: &ToolCtx, args: &Value) -> ToolResult {
        let path = match get_string(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let content = match get_string(args, "content") {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e),
        };
        let resolved = match resolve_path(&path, &ctx.workspace, ctx.restrict_to_workspace) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        if let Some(parent) = resolved.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return ToolResult::error(e.to_string());
            }
        }
        let mut f = match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)
        {
            Ok(f) => f,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        if let Err(e) = f.write_all(content.as_bytes()) {
            return ToolResult::error(e.to_string());
        }
        ToolResult::ok("appended")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_restrict_rejects_escape() {
        let ws = std::env::temp_dir();
        assert!(resolve_path("..", &ws, true).is_err());
        assert!(resolve_path("../etc/passwd", &ws, true).is_err());
    }

    #[test]
    fn read_file_roundtrip() {
        let dir = std::env::temp_dir();
        let f = dir.join("icrab_test_read_file.txt");
        let _ = fs::write(&f, "hello");
        let ctx = ToolCtx {
            workspace: dir.clone(),
            restrict_to_workspace: true,
            chat_id: None,
            channel: None,
            outbound_tx: None,
        };
        let rel = f.strip_prefix(&dir).unwrap().to_str().unwrap();
        let args = serde_json::json!({ "path": rel });
        let res = ReadFile.execute(&ctx, &args);
        assert!(!res.is_error);
        assert_eq!(res.for_llm, "hello");
        let _ = fs::remove_file(&f);
    }
}
