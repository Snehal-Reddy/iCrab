//! Vault indexer: keeps `vault_index` (and its FTS5 shadow `vault_fts`) in
//! sync with the Obsidian Markdown files on disk.
//!
//! # How it works
//!
//! [`scan_vault`] recursively walks the workspace directory, skipping
//! `.git/`, `.icrab/`, and `.obsidian/`.  For every `.md` file it finds it
//! compares the on-disk modification time against the timestamp stored in
//! `vault_index`.  If the file is new or has been modified it upserts the
//! content.  After the walk, any row in `vault_index` whose file no longer
//! exists on disk is removed (the FTS5 delete triggers handle the shadow
//! table automatically).
//!
//! # Threading
//!
//! All operations are synchronous (`std::fs`, `rusqlite`).  Call this
//! function from `tokio::task::spawn_blocking` when using from an async
//! context:
//!
//! ```ignore
//! let stats = tokio::task::spawn_blocking(move || {
//!     icrab::memory::indexer::scan_vault(&workspace, &db)
//! }).await??;
//! ```
//!
//! # Triggering
//!
//! The indexer should run:
//! - **On startup** — wired in `main.rs` immediately after the DB is opened.
//! - **After every Git sync** — called at the end of the sync task (Phase 5).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::memory::db::{BrainDb, DbError};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Directories to skip during the vault walk (relative names, not full paths).
const SKIP_DIRS: &[&str] = &[".git", ".icrab", ".obsidian"];

/// Summary of a completed vault scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanStats {
    /// Files inserted or updated in `vault_index` (content changed / first index).
    pub indexed: usize,
    /// Files already up-to-date (mtime matched stored value — skipped).
    pub skipped: usize,
    /// Stale `vault_index` rows removed (files deleted from disk since last scan).
    pub removed: usize,
}

impl std::fmt::Display for ScanStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} indexed, {} up-to-date, {} removed",
            self.indexed, self.skipped, self.removed
        )
    }
}

/// Error returned by vault indexer operations.
#[derive(Debug)]
pub struct IndexerError(pub String);

impl std::fmt::Display for IndexerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "indexer: {}", self.0)
    }
}

impl std::error::Error for IndexerError {}

impl From<DbError> for IndexerError {
    fn from(e: DbError) -> Self {
        IndexerError(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// VaultIndexer (optional struct wrapper)
// ---------------------------------------------------------------------------

/// Convenience wrapper that couples an `Arc<BrainDb>` to the indexer so it
/// can be stored in application state (e.g. for the post-git-sync trigger).
#[derive(Debug, Clone)]
pub struct VaultIndexer {
    db: Arc<BrainDb>,
}

impl VaultIndexer {
    /// Create a new indexer bound to `db`.
    pub fn new(db: Arc<BrainDb>) -> Self {
        Self { db }
    }

    /// Run the scan synchronously. Intended for `spawn_blocking`.
    pub fn scan(&self, workspace: &Path) -> Result<ScanStats, IndexerError> {
        scan_vault(workspace, &self.db)
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Synchronously scan all `.md` files under `workspace`, upsert those that
/// are new or modified, and prune DB entries whose files no longer exist.
///
/// Returns a [`ScanStats`] summary.
pub fn scan_vault(workspace: &Path, db: &BrainDb) -> Result<ScanStats, IndexerError> {
    let mut stats = ScanStats::default();
    let mut live_paths: HashSet<String> = HashSet::new();

    walk_dir(workspace, workspace, &mut live_paths, db, &mut stats)?;

    // Remove entries for files that are no longer on disk.
    stats.removed = db.delete_vault_stale(&live_paths)?;

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Recursive directory walker.  Skips dirs in [`SKIP_DIRS`] and non-`.md`
/// files.  Errors reading individual entries are logged but not fatal so that
/// one bad file doesn't abort the whole scan.
fn walk_dir(
    dir: &Path,
    workspace: &Path,
    live_paths: &mut HashSet<String>,
    db: &BrainDb,
    stats: &mut ScanStats,
) -> Result<(), IndexerError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("vault indexer: read_dir {}: {e}", dir.display());
            return Ok(());
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("vault indexer: entry error: {e}");
                continue;
            }
        };

        let path = entry.path();

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("vault indexer: metadata {}: {e}", path.display());
                continue;
            }
        };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if meta.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            walk_dir(&path, workspace, live_paths, db, stats)?;
        } else if meta.is_file() {
            // Only index Markdown files.
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            // Build a workspace-relative path with forward slashes.
            let rel = match path.strip_prefix(workspace) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };

            let mtime = mtime_unix(&meta);

            // Mark as live regardless of whether we upsert.
            live_paths.insert(rel.clone());

            // Check whether this file is already up-to-date.
            let stored = db
                .get_vault_last_modified(&rel)
                .map_err(IndexerError::from)?;

            if stored == Some(mtime) {
                stats.skipped += 1;
                continue;
            }

            // Read and upsert.
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    db.upsert_vault_entry(&rel, &content, mtime)
                        .map_err(IndexerError::from)?;
                    stats.indexed += 1;
                }
                Err(e) => {
                    // Non-UTF-8 or unreadable files: log, keep in live_paths,
                    // skip upsert.  We don't remove the old entry either.
                    eprintln!("vault indexer: read {}: {e}", path.display());
                }
            }
        }
    }

    Ok(())
}

/// Extract the modification time of a file as a Unix timestamp (seconds).
/// Returns `0` if the platform does not support `modified()`.
fn mtime_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| {
            #[allow(clippy::cast_possible_wrap)]
            let secs = d.as_secs() as i64;
            secs
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::memory::db::BrainDb;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn temp_db() -> (TempDir, Arc<BrainDb>) {
        let tmp = TempDir::new().unwrap();
        let db = Arc::new(BrainDb::open(tmp.path()).unwrap());
        (tmp, db)
    }

    /// Create a `.md` file inside `dir` with the given name and content.
    /// Returns the path to the created file.
    fn write_md(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    // ── Basic scan ───────────────────────────────────────────────────────────

    #[test]
    fn scan_empty_workspace() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();
        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.removed, 0);
    }

    #[test]
    fn scan_indexes_md_files() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "note.md", "Hello vault");
        write_md(ws.path(), "ideas.md", "Some ideas");

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 2);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.removed, 0);
    }

    #[test]
    fn scan_ignores_non_md_files() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "note.md", "a markdown file");
        std::fs::write(ws.path().join("script.sh"), "#!/bin/sh").unwrap();
        std::fs::write(ws.path().join("data.json"), "{}").unwrap();
        std::fs::write(ws.path().join("image.png"), b"\x89PNG").unwrap();

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 1, "only note.md should be indexed");
    }

    #[test]
    fn scan_recursive_subdirectories() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "root.md", "root");
        write_md(ws.path(), "Daily log/2026-02-20.md", "today's log");
        write_md(ws.path(), "Workouts/Program 1.md", "squats");
        write_md(ws.path(), "CS Learnings/Rust/Enums.md", "enums");

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 4);
    }

    // ── Skip logic ───────────────────────────────────────────────────────────

    #[test]
    fn scan_skips_git_dir() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        // Create a .git dir with a .md file inside — should be ignored.
        write_md(ws.path(), ".git/COMMIT_EDITMSG.md", "git internal");
        write_md(ws.path(), "real.md", "real note");

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 1);

        let paths = db.list_vault_filepaths().unwrap();
        assert!(!paths.iter().any(|p| p.contains(".git")));
    }

    #[test]
    fn scan_skips_icrab_dir() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), ".icrab/internal.md", "runtime state");
        write_md(ws.path(), "user.md", "user note");

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 1);
    }

    #[test]
    fn scan_skips_obsidian_dir() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), ".obsidian/config.md", "obsidian config");
        write_md(ws.path(), "vault_note.md", "actual note");

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 1);
    }

    // ── Idempotency (re-scan without changes) ────────────────────────────────

    #[test]
    fn second_scan_skips_unchanged_files() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "a.md", "content a");
        write_md(ws.path(), "b.md", "content b");

        // First scan: index both.
        let s1 = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(s1.indexed, 2);
        assert_eq!(s1.skipped, 0);

        // Second scan: both should be skipped (mtime unchanged).
        let s2 = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(s2.indexed, 0);
        assert_eq!(s2.skipped, 2);
    }

    // ── Update detection ─────────────────────────────────────────────────────

    #[test]
    fn scan_reindexes_modified_file() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        let file = write_md(ws.path(), "evolving.md", "initial_content_alpha");

        // First scan.
        scan_vault(ws.path(), &db).unwrap();

        // Force staleness: set stored mtime to 0 so next scan will re-index.
        // (On fast filesystems mtime resolution may be 1s; faking via DB is reliable.)
        db.upsert_vault_entry("evolving.md", "initial_content_alpha", 0)
            .unwrap();

        // Write DIFFERENT content — real mtime is now > 0.
        std::fs::write(&file, "updated_content_beta").unwrap();

        let s2 = scan_vault(ws.path(), &db).unwrap();
        // Real mtime > 0, stored is 0 → re-indexed.
        assert_eq!(s2.indexed, 1);

        // Verify the stored content was updated.
        let stored = db.get_vault_content("evolving.md").unwrap();
        assert_eq!(stored.as_deref(), Some("updated_content_beta"));
    }

    // ── Stale entry pruning ──────────────────────────────────────────────────

    #[test]
    fn scan_removes_stale_entries() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        let file_b = write_md(ws.path(), "keep.md", "keeper");
        write_md(ws.path(), "delete_me.md", "will be deleted");

        // Index both.
        scan_vault(ws.path(), &db).unwrap();
        assert_eq!(db.list_vault_filepaths().unwrap().len(), 2);

        // Remove the file from disk.
        std::fs::remove_file(ws.path().join("delete_me.md")).unwrap();
        drop(file_b); // keep.md still exists

        // Second scan should remove the stale entry.
        let s2 = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(s2.removed, 1);

        let paths = db.list_vault_filepaths().unwrap();
        assert_eq!(paths, vec!["keep.md"]);
    }

    #[test]
    fn scan_stale_removed_from_fts5() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "phantom.md", "unique_ghost_term_zyx");

        scan_vault(ws.path(), &db).unwrap();

        // Verify indexed.
        assert_eq!(
            db.vault_fts_count("\"unique_ghost_term_zyx\"").unwrap(),
            1,
            "should be indexed first"
        );

        // Delete from disk and re-scan.
        std::fs::remove_file(ws.path().join("phantom.md")).unwrap();
        scan_vault(ws.path(), &db).unwrap();

        // FTS5 should no longer find it.
        assert_eq!(
            db.vault_fts_count("\"unique_ghost_term_zyx\"").unwrap(),
            0,
            "deleted file should not be in FTS5"
        );
    }

    // ── Content stored in vault_index ────────────────────────────────────────

    #[test]
    fn scan_stores_correct_content() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "workout.md", "Bench press 3×5 @ 100kg");

        scan_vault(ws.path(), &db).unwrap();

        let content = db.get_vault_content("workout.md").unwrap();
        assert_eq!(content.as_deref(), Some("Bench press 3×5 @ 100kg"));
    }

    // ── Relative path format ─────────────────────────────────────────────────

    #[test]
    fn scan_stores_relative_paths() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "Daily log/2026-02-20.md", "today");

        scan_vault(ws.path(), &db).unwrap();

        let paths = db.list_vault_filepaths().unwrap();
        assert_eq!(paths, vec!["Daily log/2026-02-20.md"]);
    }

    // ── FTS5 search after indexing ────────────────────────────────────────────

    #[test]
    fn scan_enables_fts5_search() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        // Use the exact same word ("squat") in both files so FTS5 token matches.
        write_md(ws.path(), "Daily log/2026-02-20.md", "Did squat and bench press today.");
        write_md(ws.path(), "Workouts/Program.md", "Monday: squat 5x5 at 80kg");
        write_md(ws.path(), "Ideas.md", "Build an AI assistant for the iPhone.");

        scan_vault(ws.path(), &db).unwrap();

        // BM25 search via the public vault_fts_count API.
        assert_eq!(
            db.vault_fts_count("\"squat\"").unwrap(),
            2,
            "both workout files contain 'squat'"
        );
        assert_eq!(db.vault_fts_count("\"iPhone\"").unwrap(), 1);
    }

    // ── VaultIndexer struct ──────────────────────────────────────────────────

    #[test]
    fn vault_indexer_struct_scan() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "via_struct.md", "indexed via VaultIndexer");

        let indexer = VaultIndexer::new(Arc::clone(&db));
        let stats = indexer.scan(ws.path()).unwrap();
        assert_eq!(stats.indexed, 1);
    }

    // ── ScanStats Display ────────────────────────────────────────────────────

    #[test]
    fn scan_stats_display() {
        let s = ScanStats {
            indexed: 3,
            skipped: 7,
            removed: 1,
        };
        let text = s.to_string();
        assert!(text.contains("3 indexed"));
        assert!(text.contains("7 up-to-date"));
        assert!(text.contains("1 removed"));
    }

    // ── Unicode filenames and content ─────────────────────────────────────────

    #[test]
    fn scan_unicode_content() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "unicode.md", "こんにちは 🚀 Ñoño");

        scan_vault(ws.path(), &db).unwrap();

        let content = db.get_vault_content("unicode.md").unwrap();
        assert_eq!(content.as_deref(), Some("こんにちは 🚀 Ñoño"));
    }

    // ── mtime_unix helper ────────────────────────────────────────────────────

    #[test]
    fn mtime_unix_returns_nonnegative() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("test.md");
        std::fs::write(&f, "hi").unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        let t = mtime_unix(&meta);
        assert!(t >= 0);
    }

    // ── Deeply nested directories ────────────────────────────────────────────

    #[test]
    fn scan_deeply_nested() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), "a/b/c/d/deep.md", "deep content");

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 1);

        let paths = db.list_vault_filepaths().unwrap();
        assert_eq!(paths, vec!["a/b/c/d/deep.md"]);
    }

    // ── Workspace with only skip-dirs ────────────────────────────────────────

    #[test]
    fn scan_only_skip_dirs_produces_empty_index() {
        let ws = TempDir::new().unwrap();
        let (_db_tmp, db) = temp_db();

        write_md(ws.path(), ".git/blob.md", "git internal");
        write_md(ws.path(), ".icrab/state.md", "state");
        write_md(ws.path(), ".obsidian/config.md", "config");

        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 0);
        assert!(db.list_vault_filepaths().unwrap().is_empty());
    }
}
