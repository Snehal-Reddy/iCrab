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
            std::process::Command::new("git")
                .args(["pull", "--rebase", "origin", "main"])
                .current_dir(&ws)
                .output()
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
