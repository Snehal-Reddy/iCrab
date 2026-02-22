//! `grep_dir` tool: fast regex scan over `.md` files in a workspace subdirectory.
//!
//! Avoids FTS5 overhead when a skill knows the exact folder and pattern it needs.
//! Always restricted to the workspace — paths escaping via `..` are rejected.

use std::path::Path;

use regex::Regex;
use serde_json::Value;

use crate::tools::context::ToolCtx;
use crate::tools::file::resolve_path;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

/// Hard cap on returned matches to avoid overwhelming the LLM context.
const MAX_MATCHES: usize = 50;

pub struct GrepDirTool;

impl Tool for GrepDirTool {
    fn name(&self) -> &str {
        "grep_dir"
    }

    fn description(&self) -> &str {
        "Fast regex search across all .md files in a specific workspace sub-directory. \
         Use this when you know exactly which folder to look in and need precise line-level matches \
         (e.g. finding a specific date entry in 'Daily log/', or a movement in 'Workouts/'). \
         Returns matching lines with file path and line number."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex (or plain literal string) to search for."
                },
                "dir_path": {
                    "type": "string",
                    "description": "Sub-directory within the workspace to search (e.g. \"Workouts/\" or \"Daily log/\"). \
                                    Use \".\" or \"\" to search the entire workspace."
                }
            },
            "required": ["pattern", "dir_path"]
        })
    }

    fn execute<'a>(&'a self, ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let workspace = ctx.workspace.clone();
        let restrict = ctx.restrict_to_workspace;
        let args = args.clone();

        Box::pin(async move {
            let pattern = match args.get("pattern").and_then(Value::as_str) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return ToolResult::error("missing or invalid 'pattern'"),
            };

            let dir_raw = args
                .get("dir_path")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .trim()
                .to_string();

            // Treat empty string as workspace root.
            let dir_raw = if dir_raw.is_empty() {
                ".".to_string()
            } else {
                dir_raw
            };

            let re = match Regex::new(&pattern) {
                Ok(r) => r,
                Err(e) => return ToolResult::error(format!("invalid regex: {e}")),
            };

            // Resolve and validate directory path.
            let dir_path = match resolve_path(&dir_raw, &workspace, restrict).await {
                Ok(p) => p,
                Err(e) => return ToolResult::error(format!("invalid dir_path: {e}")),
            };

            match tokio::task::spawn_blocking(move || {
                grep_dir_blocking(&dir_path, &re, MAX_MATCHES, &workspace)
            })
            .await
            {
                Ok(Ok(matches)) => format_grep_results(&pattern, &dir_raw, &matches),
                Ok(Err(e)) => ToolResult::error(format!("grep failed: {e}")),
                Err(e) => ToolResult::error(format!("grep task error: {e}")),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Sync grep worker (runs in spawn_blocking)
// ---------------------------------------------------------------------------

struct GrepMatch {
    rel_path: String,
    line_no: usize,
    line: String,
}

fn grep_dir_blocking(
    dir: &Path,
    re: &Regex,
    max_matches: usize,
    workspace: &Path,
) -> Result<Vec<GrepMatch>, String> {
    if !dir.exists() {
        return Err(format!("directory not found: {}", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }

    let mut matches = Vec::new();
    walk_and_grep(dir, re, workspace, &mut matches, max_matches);
    Ok(matches)
}

fn walk_and_grep(
    dir: &Path,
    re: &Regex,
    workspace: &Path,
    matches: &mut Vec<GrepMatch>,
    max_matches: usize,
) {
    if matches.len() >= max_matches {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("grep_dir: read_dir {}: {e}", dir.display());
            return;
        }
    };

    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if matches.len() >= max_matches {
            break;
        }

        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            let name = entry.file_name();
            let n = name.to_string_lossy();
            if n.starts_with('.') {
                continue;
            }
            walk_and_grep(&path, re, workspace, matches, max_matches);
        } else if meta.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            let rel = path
                .strip_prefix(workspace)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for (line_no, line) in content.lines().enumerate() {
                if matches.len() >= max_matches {
                    break;
                }
                if re.is_match(line) {
                    matches.push(GrepMatch {
                        rel_path: rel.clone(),
                        line_no: line_no + 1,
                        line: line.to_string(),
                    });
                }
            }
        }
    }
}

fn format_grep_results(pattern: &str, dir_path: &str, matches: &[GrepMatch]) -> ToolResult {
    if matches.is_empty() {
        return ToolResult::ok(format!(
            "No matches found for pattern \"{}\" in \"{}\".",
            pattern, dir_path
        ));
    }

    let truncated = matches.len() >= MAX_MATCHES;
    let mut out = format!(
        "Found {} match(es) for \"{}\" in \"{}\"{}:\n",
        matches.len(),
        pattern,
        dir_path,
        if truncated { " (truncated)" } else { "" }
    );
    for m in matches {
        out.push_str(&format!("\n{}:{}: {}", m.rel_path, m.line_no, m.line));
    }
    ToolResult::ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    use crate::tools::context::ToolCtx;
    use crate::tools::registry::Tool;

    fn tmp_ctx(ws: &Path) -> ToolCtx {
        ToolCtx {
            workspace: ws.to_path_buf(),
            restrict_to_workspace: true,
            chat_id: None,
            channel: None,
            outbound_tx: None,
        }
    }

    fn write_md(dir: &Path, name: &str, content: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn tool_name() {
        assert_eq!(GrepDirTool.name(), "grep_dir");
    }

    #[test]
    fn tool_params_require_pattern_and_dir() {
        let params = GrepDirTool.parameters();
        let req = params["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "pattern"));
        assert!(req.iter().any(|v| v == "dir_path"));
    }

    #[tokio::test]
    async fn missing_pattern_returns_error() {
        let tmp = TempDir::new().unwrap();
        let res = GrepDirTool
            .execute(
                &tmp_ctx(tmp.path()),
                &serde_json::json!({ "dir_path": "." }),
            )
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn invalid_regex_returns_error() {
        let tmp = TempDir::new().unwrap();
        let res = GrepDirTool
            .execute(
                &tmp_ctx(tmp.path()),
                &serde_json::json!({ "pattern": "[unclosed", "dir_path": "." }),
            )
            .await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("invalid regex"));
    }

    #[tokio::test]
    async fn no_match_returns_no_matches_message() {
        let tmp = TempDir::new().unwrap();
        write_md(tmp.path(), "note.md", "hello world");

        let res = GrepDirTool
            .execute(
                &tmp_ctx(tmp.path()),
                &serde_json::json!({ "pattern": "squats", "dir_path": "." }),
            )
            .await;
        assert!(!res.is_error);
        assert!(res.for_llm.contains("No matches"));
    }

    #[tokio::test]
    async fn finds_matching_line() {
        let tmp = TempDir::new().unwrap();
        write_md(
            tmp.path(),
            "Workouts/program.md",
            "Monday: squats 5x5\nTuesday: bench press",
        );

        let res = GrepDirTool
            .execute(
                &tmp_ctx(tmp.path()),
                &serde_json::json!({ "pattern": "squats", "dir_path": "Workouts/" }),
            )
            .await;
        assert!(!res.is_error, "{}", res.for_llm);
        assert!(res.for_llm.contains("squats"));
        assert!(res.for_llm.contains("program.md"));
    }

    #[tokio::test]
    async fn path_escape_rejected() {
        let tmp = TempDir::new().unwrap();
        let res = GrepDirTool
            .execute(
                &tmp_ctx(tmp.path()),
                &serde_json::json!({ "pattern": "x", "dir_path": "../../etc" }),
            )
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn empty_dir_path_searches_workspace() {
        let tmp = TempDir::new().unwrap();
        write_md(tmp.path(), "note.md", "squats workout");

        let res = GrepDirTool
            .execute(
                &tmp_ctx(tmp.path()),
                &serde_json::json!({ "pattern": "squats", "dir_path": "" }),
            )
            .await;
        assert!(!res.is_error, "{}", res.for_llm);
        assert!(res.for_llm.contains("note.md"));
    }

    #[tokio::test]
    async fn missing_dir_returns_error() {
        let tmp = TempDir::new().unwrap();
        let res = GrepDirTool
            .execute(
                &tmp_ctx(tmp.path()),
                &serde_json::json!({ "pattern": "x", "dir_path": "nonexistent_dir/" }),
            )
            .await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("not found") || res.for_llm.contains("directory"));
    }

    #[test]
    fn grep_blocking_finds_match() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.md");
        std::fs::write(&file, "line one\nsquats here\nline three").unwrap();

        let re = Regex::new("squats").unwrap();
        let matches = grep_dir_blocking(tmp.path(), &re, 50, tmp.path()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_no, 2);
        assert!(matches[0].line.contains("squats"));
    }

    #[test]
    fn grep_blocking_nonexistent_dir_errors() {
        let tmp = TempDir::new().unwrap();
        let bad = tmp.path().join("nope");
        let re = Regex::new("x").unwrap();
        let result = grep_dir_blocking(&bad, &re, 50, tmp.path());
        assert!(result.is_err());
    }
}
