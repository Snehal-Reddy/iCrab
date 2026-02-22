//! `sync_vault` tool: explicit git sync for the Obsidian vault.
//!
//! Runs in the workspace directory:
//!   `git pull --rebase origin main`
//!   `git add .`
//!   `git commit -m "<message>"`
//!   `git push origin main`
//!
//! The LLM calls this at logical endpoints (end of a workout log, etc.)
//! rather than on every file edit, keeping the agent non-blocking.

use std::process::Output;

use serde_json::Value;

use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

pub struct GitSyncTool;

impl Tool for GitSyncTool {
    fn name(&self) -> &str {
        "sync_vault"
    }

    fn description(&self) -> &str {
        "Sync the Obsidian vault with GitHub: pull latest changes, stage all edits, \
         commit with your message, and push. Call this at the end of a task that modifies \
         vault files (e.g. after logging a workout or updating a note) to keep the vault \
         consistent across devices."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "commit_message": {
                    "type": "string",
                    "description": "Short commit message describing the changes (e.g. 'Log workout 2026-02-21')."
                }
            },
            "required": ["commit_message"]
        })
    }

    fn execute<'a>(&'a self, ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let workspace = ctx.workspace.clone();
        let args = args.clone();

        Box::pin(async move {
            let msg = match args.get("commit_message").and_then(Value::as_str) {
                Some(m) if !m.trim().is_empty() => m.trim().to_string(),
                _ => return ToolResult::error("missing or invalid 'commit_message'"),
            };

            let mut log = String::new();

            // Step 1: pull
            match run_git(&workspace, &["pull", "--rebase", "origin", "main"]).await {
                Ok(out) => append_output(&mut log, "git pull", &out),
                Err(e) => return ToolResult::error(format!("git pull failed: {e}")),
            }

            // Step 2: stage
            match run_git(&workspace, &["add", "."]).await {
                Ok(out) => append_output(&mut log, "git add", &out),
                Err(e) => return ToolResult::error(format!("git add failed: {e}")),
            }

            // Step 3: commit (non-fatal if nothing to commit)
            match run_git(&workspace, &["commit", "-m", &msg]).await {
                Ok(out) => append_output(&mut log, "git commit", &out),
                Err(e) => {
                    log.push_str(&format!("\ngit commit: {e}"));
                }
            }

            // Step 4: push
            match run_git(&workspace, &["push", "origin", "main"]).await {
                Ok(out) => append_output(&mut log, "git push", &out),
                Err(e) => return ToolResult::error(format!("git push failed: {e}\n\n{log}")),
            }

            ToolResult::ok(log.trim().to_string())
        })
    }
}

async fn run_git(workspace: &std::path::Path, args: &[&str]) -> Result<Output, String> {
    let workspace = workspace.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();

    tokio::task::spawn_blocking(move || {
        // SAFETY: `system` is a standard POSIX libc function. Its C signature is
        // `int system(const char *command)`. We correctly map `const char *` to
        // `*const std::ffi::c_char` and `int` to `std::ffi::c_int`.
        unsafe extern "C" {
            fn system(command: *const std::ffi::c_char) -> std::ffi::c_int;
        }

        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let temp_dir = std::env::temp_dir();
        let pid = std::process::id();
        let c = COUNTER.fetch_add(1, Ordering::SeqCst);

        let out_file = temp_dir.join(format!("icrab_git_tool_{pid}_{c}.out"));
        let err_file = temp_dir.join(format!("icrab_git_tool_{pid}_{c}.err"));

        fn escape_sh(s: &str) -> String {
            format!("'{}'", s.replace("'", "'\\''"))
        }

        let escaped_args: Vec<String> = args.iter().map(|s| escape_sh(s)).collect();
        let cmd_str = format!(
            "cd {} && git {} > {} 2> {}",
            escape_sh(workspace.to_str().unwrap_or(".")),
            escaped_args.join(" "),
            escape_sh(out_file.to_str().unwrap()),
            escape_sh(err_file.to_str().unwrap())
        );

        let c_cmd = std::ffi::CString::new(cmd_str).map_err(|e| e.to_string())?;
        // SAFETY: `c_cmd` is a valid, null-terminated C string created by `CString::new`.
        // The pointer remains valid for the duration of the `system` call.
        let status = unsafe { system(c_cmd.as_ptr()) };

        let stdout = std::fs::read(&out_file).unwrap_or_default();
        let stderr = std::fs::read(&err_file).unwrap_or_default();

        let _ = std::fs::remove_file(&out_file);
        let _ = std::fs::remove_file(&err_file);

        use std::os::unix::process::ExitStatusExt;
        let exit_status = std::process::ExitStatus::from_raw(status);

        Ok::<std::process::Output, String>(std::process::Output {
            status: exit_status,
            stdout,
            stderr,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

fn append_output(log: &mut String, label: &str, out: &Output) {
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let status = if out.status.success() { "ok" } else { "failed" };
    log.push_str(&format!("\n[{label}: {status}]"));
    if !stdout.is_empty() {
        log.push('\n');
        log.push_str(&stdout);
    }
    if !stderr.is_empty() {
        log.push('\n');
        log.push_str(&stderr);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::context::ToolCtx;
    use crate::tools::registry::Tool;

    fn dummy_ctx() -> ToolCtx {
        ToolCtx {
            workspace: std::env::temp_dir(),
            restrict_to_workspace: true,
            chat_id: None,
            channel: None,
            outbound_tx: None,
        }
    }

    #[test]
    fn tool_name_and_description() {
        assert_eq!(GitSyncTool.name(), "sync_vault");
        assert!(GitSyncTool.description().to_lowercase().contains("commit"));
    }

    #[test]
    fn parameters_require_commit_message() {
        let params = GitSyncTool.parameters();
        assert_eq!(params["required"][0], "commit_message");
    }

    #[tokio::test]
    async fn missing_commit_message_returns_error() {
        let res = GitSyncTool
            .execute(&dummy_ctx(), &serde_json::json!({}))
            .await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("commit_message"));
    }

    #[tokio::test]
    async fn blank_commit_message_returns_error() {
        let res = GitSyncTool
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "commit_message": "   " }),
            )
            .await;
        assert!(res.is_error);
    }
}
