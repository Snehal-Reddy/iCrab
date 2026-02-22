//! Background git pull loop: keeps the local Obsidian vault clone in sync
//! with GitHub and triggers vault re-indexing after each successful pull.
//!
//! Chat history (`brain.db`) is strictly local and is never pushed to Git.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::memory::db::BrainDb;
use crate::memory::indexer::VaultIndexer;

/// Default interval between background pulls (3 hours).
pub const DEFAULT_PULL_INTERVAL_SECS: u64 = 3 * 60 * 60;

/// Spawn a background task that periodically runs `git pull --rebase origin
/// main` in `workspace`, then re-scans the vault FTS5 index.
///
/// Errors are logged but never fatal — the app keeps running regardless.
pub fn spawn_git_pull_loop(workspace: PathBuf, db: Arc<BrainDb>, interval_secs: u64) {
    tokio::spawn(pull_loop(workspace, db, interval_secs));
}

async fn pull_loop(workspace: PathBuf, db: Arc<BrainDb>, interval_secs: u64) {
    let indexer = VaultIndexer::new(db);
    let interval = Duration::from_secs(interval_secs);

    loop {
        tokio::time::sleep(interval).await;

        let ws = workspace.clone();
        let output_res = tokio::task::spawn_blocking(move || {
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

            let out_file = temp_dir.join(format!("icrab_git_sync_{pid}_{c}.out"));
            let err_file = temp_dir.join(format!("icrab_git_sync_{pid}_{c}.err"));

            fn escape_sh(s: &str) -> String {
                format!("'{}'", s.replace("'", "'\\''"))
            }

            let cmd_str = format!(
                "cd {} && git pull --rebase origin main > {} 2> {}",
                escape_sh(ws.to_str().unwrap_or(".")),
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
        .await;

        match output_res {
            Ok(Ok(out)) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                eprintln!("git pull: ok — {}", stdout.trim());

                let ws_reindex = workspace.clone();
                // Re-index vault so FTS5 reflects any new notes from PC.
                let idx = indexer.clone();
                match tokio::task::spawn_blocking(move || idx.scan(&ws_reindex)).await {
                    Ok(Ok(stats)) => eprintln!("vault re-index: {stats}"),
                    Ok(Err(e)) => eprintln!("vault re-index warning: {e}"),
                    Err(e) => eprintln!("vault re-index task error: {e}"),
                }
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "git pull: non-zero exit ({}): {}",
                    out.status,
                    stderr.trim()
                );
            }
            Ok(Err(e)) => eprintln!("git pull: failed to spawn: {e}"),
            Err(e) => eprintln!("git pull: task panicked: {e}"),
        }
    }
}
