//! Integration tests for the vault indexer (Phase 3).
//!
//! These tests run against a real temporary filesystem and a real BrainDb
//! (in-memory-equivalent via tempfile), exercising the full scan → SQLite →
//! FTS5 pipeline end-to-end.

use std::sync::Arc;
use tempfile::TempDir;

use icrab::memory::db::BrainDb;
use icrab::memory::indexer::{ScanStats, VaultIndexer, scan_vault};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn setup() -> (TempDir, TempDir, Arc<BrainDb>) {
    let ws = TempDir::new().unwrap();
    let db_tmp = TempDir::new().unwrap();
    let db = Arc::new(BrainDb::open(db_tmp.path()).unwrap());
    (ws, db_tmp, db)
}

fn write_md(dir: &std::path::Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, content).unwrap();
}

// ---------------------------------------------------------------------------
// Scan fundamentals
// ---------------------------------------------------------------------------

#[test]
fn integration_empty_workspace_no_crash() {
    let (ws, _db_tmp, db) = setup();
    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats, ScanStats::default());
}

#[test]
fn integration_single_note_indexed() {
    let (ws, _db_tmp, db) = setup();
    write_md(ws.path(), "TODO.md", "- Buy milk\n- Finish project");

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 1);
    assert_eq!(stats.skipped, 0);
    assert_eq!(stats.removed, 0);
}

#[test]
fn integration_full_vault_structure() {
    let (ws, _db_tmp, db) = setup();

    // Simulate a realistic Obsidian vault layout.
    write_md(ws.path(), "IDEAS.md", "Build a personal AI.");
    write_md(ws.path(), "TODO.md", "Deploy iCrab to iSH.");
    write_md(ws.path(), "Daily log/2026-02-19.md", "Ran 5km. Felt great.");
    write_md(ws.path(), "Daily log/2026-02-20.md", "Bench press PR: 110kg.");
    write_md(ws.path(), "Workouts/Mixed Program 1.md", "Monday: Squat 5×5.");
    write_md(ws.path(), "CS Learnings/Rust/Enums.md", "enum Color { Red, Green, Blue }");
    write_md(ws.path(), "CS Learnings/Rust/Traits.md", "trait Animal { fn sound(&self); }");
    write_md(ws.path(), "CS Learnings/DSA/BFS.md", "Breadth-first search uses a queue.");

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 8);
}

// ---------------------------------------------------------------------------
// Skip-directory enforcement
// ---------------------------------------------------------------------------

#[test]
fn integration_skip_git_dir() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), ".git/config.md", "git internals");
    write_md(ws.path(), ".git/refs/HEAD.md", "ref: refs/heads/main");
    write_md(ws.path(), "user_note.md", "visible");

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 1, "only user_note.md");

    let paths = db.list_vault_filepaths().unwrap();
    assert!(!paths.iter().any(|p| p.contains(".git")));
    assert!(paths.iter().any(|p| p == "user_note.md"));
}

#[test]
fn integration_skip_icrab_dir() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), ".icrab/brain_notes.md", "runtime state");
    write_md(ws.path(), "personal.md", "diary entry");

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 1);
    assert_eq!(db.list_vault_filepaths().unwrap(), vec!["personal.md"]);
}

#[test]
fn integration_skip_obsidian_dir() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), ".obsidian/workspace.md", "UI state");
    write_md(ws.path(), ".obsidian/plugins/README.md", "plugin docs");
    write_md(ws.path(), "recipe.md", "Pasta carbonara");

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 1);
    assert_eq!(db.list_vault_filepaths().unwrap(), vec!["recipe.md"]);
}

#[test]
fn integration_all_skip_dirs_together() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), ".git/COMMIT_MSG.md", "git");
    write_md(ws.path(), ".icrab/state.md", "icrab");
    write_md(ws.path(), ".obsidian/config.md", "obsidian");
    write_md(ws.path(), "visible.md", "this counts");

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 1);
}

// ---------------------------------------------------------------------------
// Non-markdown file filtering
// ---------------------------------------------------------------------------

#[test]
fn integration_only_md_files_indexed() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), "note.md", "actual note");
    std::fs::write(ws.path().join("README"), "plain text").unwrap();
    std::fs::write(ws.path().join("config.toml"), "[config]").unwrap();
    std::fs::write(ws.path().join("script.sh"), "#!/bin/sh").unwrap();
    std::fs::write(ws.path().join("image.jpg"), b"\xff\xd8\xff").unwrap();

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 1);
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

#[test]
fn integration_second_scan_is_idempotent() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), "daily.md", "workout log");
    write_md(ws.path(), "ideas.md", "new business idea");

    let s1 = scan_vault(ws.path(), &db).unwrap();
    let s2 = scan_vault(ws.path(), &db).unwrap();
    let s3 = scan_vault(ws.path(), &db).unwrap();

    assert_eq!(s1.indexed, 2);
    // Second and third scans: no changes → everything skipped.
    assert_eq!(s2.indexed, 0);
    assert_eq!(s2.skipped, 2);
    assert_eq!(s3.indexed, 0);
    assert_eq!(s3.skipped, 2);
}

// ---------------------------------------------------------------------------
// Stale entry removal
// ---------------------------------------------------------------------------

#[test]
fn integration_deleted_file_pruned_from_index() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), "permanent.md", "always here");
    write_md(ws.path(), "temporary.md", "short-lived note");

    // First scan: both indexed.
    scan_vault(ws.path(), &db).unwrap();
    assert_eq!(db.list_vault_filepaths().unwrap().len(), 2);

    // Remove one file.
    std::fs::remove_file(ws.path().join("temporary.md")).unwrap();

    // Second scan: stale entry removed.
    let s2 = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(s2.removed, 1);
    assert_eq!(db.list_vault_filepaths().unwrap(), vec!["permanent.md"]);
}

#[test]
fn integration_deleted_file_removed_from_fts5() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), "ghost.md", "haunted_term_abc123");

    scan_vault(ws.path(), &db).unwrap();

    assert_eq!(
        db.vault_fts_count("\"haunted_term_abc123\"").unwrap(),
        1,
        "should be findable before deletion"
    );

    // Delete from disk, re-scan.
    std::fs::remove_file(ws.path().join("ghost.md")).unwrap();
    scan_vault(ws.path(), &db).unwrap();

    assert_eq!(
        db.vault_fts_count("\"haunted_term_abc123\"").unwrap(),
        0,
        "deleted file should be removed from FTS5"
    );
}

// ---------------------------------------------------------------------------
// FTS5 search correctness after indexing
// ---------------------------------------------------------------------------

#[test]
fn integration_fts5_bm25_search() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), "Daily log/2026-02-19.md", "Did squat and bench press today.");
    write_md(ws.path(), "Workouts/Program.md", "Monday: squat 5x5 at 80kg, bench 3x8.");
    write_md(ws.path(), "Ideas.md", "Build a personal AI assistant.");

    scan_vault(ws.path(), &db).unwrap();

    // "squat" appears in two files.
    assert_eq!(db.vault_fts_count("\"squat\"").unwrap(), 2);

    // "AI" appears in one file.
    assert_eq!(db.vault_fts_count("\"AI\"").unwrap(), 1);

    // "deadlift" appears in no files.
    assert_eq!(db.vault_fts_count("\"deadlift\"").unwrap(), 0);
}

#[test]
fn integration_fts5_snippet_query() {
    let (ws, _db_tmp, db) = setup();

    write_md(
        ws.path(),
        "CS Learnings/Rust/Enums.md",
        "In Rust, an enum can hold data. Example: enum Shape { Circle(f64), Rect(f64, f64) }.",
    );

    scan_vault(ws.path(), &db).unwrap();

    // vault_fts_search returns (filepath, snippet) pairs.
    let results = db.vault_fts_search("\"enum\"", 5).unwrap();
    assert_eq!(results.len(), 1);

    let (_fp, snippet) = &results[0];
    // The snippet should contain bold markers around the match.
    assert!(
        snippet.contains("**"),
        "snippet should highlight match: {snippet}"
    );
}

// ---------------------------------------------------------------------------
// VaultIndexer struct
// ---------------------------------------------------------------------------

#[test]
fn integration_vault_indexer_struct() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), "note1.md", "first");
    write_md(ws.path(), "note2.md", "second");

    let indexer = VaultIndexer::new(Arc::clone(&db));
    let stats = indexer.scan(ws.path()).unwrap();
    assert_eq!(stats.indexed, 2);

    // Re-scan via function API: should all be skipped.
    let stats2 = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats2.skipped, 2);
    assert_eq!(stats2.indexed, 0);
}

// ---------------------------------------------------------------------------
// Persistence across BrainDb reopen
// ---------------------------------------------------------------------------

#[test]
fn integration_index_persists_across_reopen() {
    let ws = TempDir::new().unwrap();
    let db_tmp = TempDir::new().unwrap();

    write_md(ws.path(), "persist.md", "this note must survive");

    {
        let db = Arc::new(BrainDb::open(db_tmp.path()).unwrap());
        let stats = scan_vault(ws.path(), &db).unwrap();
        assert_eq!(stats.indexed, 1);
    }

    // Reopen the DB — indexed entry must still be there.
    let db2 = Arc::new(BrainDb::open(db_tmp.path()).unwrap());
    let paths = db2.list_vault_filepaths().unwrap();
    assert_eq!(paths, vec!["persist.md"]);

    // Re-scan: should skip (already indexed, mtime unchanged).
    let stats2 = scan_vault(ws.path(), &db2).unwrap();
    assert_eq!(stats2.skipped, 1);
    assert_eq!(stats2.indexed, 0);
}

// ---------------------------------------------------------------------------
// Unicode content
// ---------------------------------------------------------------------------

#[test]
fn integration_unicode_content_round_trips() {
    let (ws, _db_tmp, db) = setup();

    let content = "## 今日の記録\n\nベンチプレス 100kg × 5rep 🏋️\n\nÑoño test.";
    write_md(ws.path(), "Daily log/日記.md", content);

    scan_vault(ws.path(), &db).unwrap();

    // The filepath stored will be e.g. "Daily log/日記.md".
    let paths = db.list_vault_filepaths().unwrap();
    let fp = paths.iter().find(|p| p.contains("日記")).expect("path not found");
    let stored_content = db.get_vault_content(fp).unwrap().expect("content not found");
    assert_eq!(stored_content, content);
}

// ---------------------------------------------------------------------------
// Large content (stress)
// ---------------------------------------------------------------------------

#[test]
fn integration_large_file_indexed() {
    let (ws, _db_tmp, db) = setup();

    // 500 KB of content — should not OOM on iSH.
    let big_content: String = "# Big note\n\n".to_string()
        + &"Lorem ipsum dolor sit amet consectetur. "
            .repeat(12_000);
    write_md(ws.path(), "big.md", &big_content);

    let stats = scan_vault(ws.path(), &db).unwrap();
    assert_eq!(stats.indexed, 1);
}

// ---------------------------------------------------------------------------
// Concurrent scan safety (same DB, two indexers)
// ---------------------------------------------------------------------------

#[test]
fn integration_sequential_scans_on_shared_db() {
    let (ws, _db_tmp, db) = setup();

    write_md(ws.path(), "shared.md", "shared content");

    // Two VaultIndexers sharing the same Arc<BrainDb>.
    let idx1 = VaultIndexer::new(Arc::clone(&db));
    let idx2 = VaultIndexer::new(Arc::clone(&db));

    let s1 = idx1.scan(ws.path()).unwrap();
    let s2 = idx2.scan(ws.path()).unwrap();

    assert_eq!(s1.indexed, 1);
    // Second scan sees the mtime already recorded.
    assert_eq!(s2.skipped, 1);
    assert_eq!(s2.indexed, 0);
}
