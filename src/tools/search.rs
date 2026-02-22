//! `search_vault` tool: BM25 keyword search over the indexed Obsidian vault.
//!
//! The tool wraps [`BrainDb::vault_fts_search`], which executes:
//! ```sql
//! SELECT filepath, snippet(vault_fts, -1, '**', '**', '...', 10) AS snip
//! FROM vault_fts
//! WHERE vault_fts MATCH ?1
//! ORDER BY bm25(vault_fts)
//! LIMIT ?2
//! ```
//!
//! # Query handling
//!
//! The raw query string from the LLM is passed to FTS5 directly.  FTS5
//! supports natural multi-word queries (default AND), OR, NOT, phrase syntax
//! (`"bench press"`), and prefix queries (`squat*`).  If FTS5 rejects the
//! query as syntactically invalid, the tool falls back to quoting each word
//! individually joined by OR, which is always safe.
//!
//! # Registration
//!
//! ```ignore
//! registry.register(SearchVaultTool::new(Arc::clone(&db)));
//! ```
//!
//! The tool holds `Arc<BrainDb>` directly — no changes to `ToolCtx` needed.

use std::sync::Arc;

use serde_json::Value;

use crate::memory::db::{BrainDb, DbError};
use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

/// Maximum number of vault search results returned to the LLM.
const DEFAULT_LIMIT: usize = 5;

// ---------------------------------------------------------------------------
// SearchVaultTool
// ---------------------------------------------------------------------------

/// Search the indexed Obsidian vault using FTS5 BM25 ranking.
pub struct SearchVaultTool {
    db: Arc<BrainDb>,
}

impl SearchVaultTool {
    /// Create a new search tool backed by `db`.
    pub fn new(db: Arc<BrainDb>) -> Self {
        Self { db }
    }
}

impl Tool for SearchVaultTool {
    fn name(&self) -> &str {
        "search_vault"
    }

    fn description(&self) -> &str {
        "Search the Obsidian vault notes for a keyword query. \
         Returns BM25-ranked file paths and matching context snippets. \
         Use this to find relevant notes before reading them in full with read_file."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to search for. \
                        Supports multi-word queries ('bench press'), \
                        prefix wildcards ('squat*'), \
                        phrases ('\"bench press\"'), \
                        and boolean operators (OR, NOT)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default 5, max 20).",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        })
    }

    fn execute<'a>(&'a self, _ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let db = Arc::clone(&self.db);
        let args = args.clone();

        Box::pin(async move {
            let query = match args.get("query").and_then(Value::as_str) {
                Some(q) => q.trim().to_string(),
                None => return ToolResult::error("missing or invalid 'query'"),
            };

            if query.is_empty() {
                return ToolResult::error("'query' must not be empty");
            }

            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_LIMIT, |v| (v as usize).clamp(1, 20));

            // vault_fts_search is synchronous (rusqlite); run off the async
            // thread pool so we don't block the Tokio executor.
            let result =
                tokio::task::spawn_blocking(move || search_with_fallback(&db, &query, limit)).await;

            match result {
                Ok(Ok(rows)) => format_results(&rows),
                Ok(Err(e)) => ToolResult::error(format!("search failed: {e}")),
                Err(e) => ToolResult::error(format!("search task error: {e}")),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run an FTS5 search.  If the query string is syntactically invalid (FTS5
/// returns an error), fall back to quoting each whitespace-separated word and
/// joining with OR — this is always a valid FTS5 query.
fn search_with_fallback(
    db: &BrainDb,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, String)>, DbError> {
    match db.vault_fts_search(query, limit) {
        Ok(rows) => Ok(rows),
        Err(_) => {
            let safe: String = query
                .split_whitespace()
                .filter(|w| !w.is_empty())
                // Strip any embedded quotes to avoid re-breaking FTS5 syntax.
                .map(|w| format!("\"{}\"", w.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" OR ");

            if safe.is_empty() {
                Ok(Vec::new())
            } else {
                db.vault_fts_search(&safe, limit)
            }
        }
    }
}

/// Format `(filepath, snippet)` pairs into a concise string for the LLM.
///
/// Output example:
/// ```text
/// Found 2 result(s) for your query:
///
/// 1. Workouts/Program.md
///    ...Monday: **squat** 5×5 at 80kg...
///
/// 2. Daily log/2026-02-20.md
///    ...Did **squat** and bench press today...
/// ```
fn format_results(rows: &[(String, String)]) -> ToolResult {
    if rows.is_empty() {
        return ToolResult::ok("No matching notes found in the vault.");
    }

    let mut out = format!("Found {} result(s):\n", rows.len());
    for (i, (filepath, snippet)) in rows.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", i + 1, filepath, snippet));
    }
    ToolResult::ok(out)
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
    use crate::tools::context::ToolCtx;
    use crate::tools::registry::Tool;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn temp_db() -> (TempDir, Arc<BrainDb>) {
        let tmp = TempDir::new().unwrap();
        let db = Arc::new(BrainDb::open(tmp.path()).unwrap());
        (tmp, db)
    }

    fn dummy_ctx() -> ToolCtx {
        ToolCtx {
            workspace: std::env::temp_dir(),
            restrict_to_workspace: true,
            chat_id: None,
            channel: None,
            outbound_tx: None,
        }
    }

    fn index(db: &BrainDb, filepath: &str, content: &str) {
        db.upsert_vault_entry(filepath, content, 0).unwrap();
    }

    // ── Metadata ─────────────────────────────────────────────────────────────

    #[test]
    fn tool_name() {
        let (_tmp, db) = temp_db();
        assert_eq!(SearchVaultTool::new(db).name(), "search_vault");
    }

    #[test]
    fn tool_description_mentions_vault() {
        let (_tmp, db) = temp_db();
        let desc = SearchVaultTool::new(db).description().to_lowercase();
        assert!(desc.contains("vault"));
    }

    #[test]
    fn tool_parameters_require_query() {
        let (_tmp, db) = temp_db();
        let params = SearchVaultTool::new(db).parameters();
        assert_eq!(params["required"][0], "query");
        assert!(params["properties"]["query"].is_object());
    }

    // ── Argument validation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn missing_query_returns_error() {
        let (_tmp, db) = temp_db();
        let tool = SearchVaultTool::new(db);
        let res = tool.execute(&dummy_ctx(), &serde_json::json!({})).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("query"));
    }

    #[tokio::test]
    async fn empty_query_returns_error() {
        let (_tmp, db) = temp_db();
        let tool = SearchVaultTool::new(db);
        let res = tool
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "  " }))
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn null_query_returns_error() {
        let (_tmp, db) = temp_db();
        let tool = SearchVaultTool::new(db);
        let res = tool
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "query": serde_json::Value::Null }),
            )
            .await;
        assert!(res.is_error);
    }

    // ── Empty vault ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_vault_returns_no_matches_message() {
        let (_tmp, db) = temp_db();
        let tool = SearchVaultTool::new(db);
        let res = tool
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "squat" }))
            .await;
        assert!(!res.is_error);
        assert!(res.for_llm.contains("No matching notes"));
    }

    // ── Successful search ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn search_finds_indexed_note() {
        let (_tmp, db) = temp_db();
        index(&db, "workout.md", "Bench press 5x5 at 100kg.");

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "bench" }))
            .await;
        assert!(!res.is_error, "expected success: {}", res.for_llm);
        assert!(res.for_llm.contains("workout.md"));
    }

    #[tokio::test]
    async fn search_result_includes_filepath_and_snippet() {
        let (_tmp, db) = temp_db();
        index(
            &db,
            "Daily log/2026-02-20.md",
            "Ran 5km today and felt great.",
        );

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "ran" }))
            .await;
        assert!(!res.is_error);
        // Filepath should appear in output.
        assert!(res.for_llm.contains("Daily log/2026-02-20.md"));
        // FTS5 snippet should include bold markers.
        assert!(res.for_llm.contains("**"));
    }

    #[tokio::test]
    async fn search_result_count_line() {
        let (_tmp, db) = temp_db();
        index(&db, "a.md", "apple cider vinegar");
        index(&db, "b.md", "apple pie recipe");

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "apple" }))
            .await;
        assert!(!res.is_error);
        assert!(
            res.for_llm.contains("Found 2 result"),
            "expected count line: {}",
            res.for_llm
        );
    }

    #[tokio::test]
    async fn search_no_match_returns_no_results_message() {
        let (_tmp, db) = temp_db();
        index(&db, "note.md", "Rust programming is fun.");

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "javascript" }))
            .await;
        assert!(!res.is_error);
        assert!(res.for_llm.contains("No matching notes"));
    }

    // ── Result limit ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn search_respects_default_limit() {
        let (_tmp, db) = temp_db();
        for i in 0..10 {
            index(
                &db,
                &format!("note{i}.md"),
                &format!("common keyword item {i}"),
            );
        }

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "query": "common keyword" }),
            )
            .await;
        assert!(!res.is_error);

        // At most DEFAULT_LIMIT (5) results.
        let count = res.for_llm.matches("\n1. ").count()
            + res.for_llm.matches("\n2. ").count()
            + res.for_llm.matches("\n3. ").count()
            + res.for_llm.matches("\n4. ").count()
            + res.for_llm.matches("\n5. ").count();
        let total_numbered = res
            .for_llm
            .lines()
            .filter(|l| {
                l.starts_with("1. ")
                    || l.starts_with("2. ")
                    || l.starts_with("3. ")
                    || l.starts_with("4. ")
                    || l.starts_with("5. ")
                    || l.starts_with("6. ")
            })
            .count();
        assert!(
            total_numbered <= DEFAULT_LIMIT,
            "expected at most {DEFAULT_LIMIT} results, got {total_numbered}: {}",
            res.for_llm
        );
        // suppress unused warning
        let _ = count;
    }

    // ── FTS5 syntax fallback ──────────────────────────────────────────────────

    #[tokio::test]
    async fn invalid_fts5_query_falls_back_gracefully() {
        let (_tmp, db) = temp_db();
        index(&db, "note.md", "hello world content");

        let tool = SearchVaultTool::new(Arc::clone(&db));
        // Syntactically invalid FTS5 query — should fall back, not error.
        let res = tool
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "AND OR NOT" }))
            .await;
        // Must not return is_error (the fallback handles invalid syntax).
        assert!(!res.is_error, "unexpected error: {}", res.for_llm);
    }

    #[tokio::test]
    async fn query_with_fts5_special_chars_does_not_panic() {
        let (_tmp, db) = temp_db();
        index(&db, "note.md", "function call test");

        let tool = SearchVaultTool::new(Arc::clone(&db));
        // These have special meaning in FTS5 and might cause syntax errors.
        for bad_query in &["(unclosed", "\"", "***", "a NEAR/999999 b"] {
            let res = tool
                .execute(&dummy_ctx(), &serde_json::json!({ "query": bad_query }))
                .await;
            assert!(
                !res.is_error,
                "query '{bad_query}' should not produce a tool error: {}",
                res.for_llm
            );
        }
    }

    // ── search_with_fallback unit ─────────────────────────────────────────────

    #[test]
    fn search_with_fallback_returns_empty_for_empty_vault() {
        let (_tmp, db) = temp_db();
        let rows = search_with_fallback(&db, "anything", 5).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn search_with_fallback_finds_indexed_content() {
        let (_tmp, db) = temp_db();
        db.upsert_vault_entry("ideas.md", "Build a Rust AI assistant.", 0)
            .unwrap();

        let rows = search_with_fallback(&db, "Rust", 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "ideas.md");
    }

    // ── LLM-configurable limit ────────────────────────────────────────────────

    #[tokio::test]
    async fn limit_parameter_respected() {
        let (_tmp, db) = temp_db();
        for i in 0..8 {
            index(
                &db,
                &format!("note{i}.md"),
                &format!("common keyword item {i}"),
            );
        }

        let tool = SearchVaultTool::new(Arc::clone(&db));

        // Ask for 3 results.
        let res = tool
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "query": "common keyword", "limit": 3 }),
            )
            .await;
        assert!(!res.is_error);
        assert!(
            res.for_llm.contains("Found 3 result"),
            "expected 3 results: {}",
            res.for_llm
        );
    }

    #[tokio::test]
    async fn limit_defaults_to_five_when_omitted() {
        let (_tmp, db) = temp_db();
        for i in 0..8 {
            index(&db, &format!("n{i}.md"), &format!("common keyword {i}"));
        }

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "query": "common keyword" }),
            )
            .await;
        assert!(!res.is_error);
        assert!(
            res.for_llm.contains("Found 5 result"),
            "expected default 5: {}",
            res.for_llm
        );
    }

    #[tokio::test]
    async fn limit_clamped_to_twenty() {
        let (_tmp, db) = temp_db();
        for i in 0..25 {
            index(&db, &format!("n{i}.md"), &format!("common keyword {i}"));
        }

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "query": "common keyword", "limit": 999 }),
            )
            .await;
        assert!(!res.is_error);
        // Should be clamped to 20.
        let result_count: usize = res
            .for_llm
            .lines()
            .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .count();
        assert!(
            result_count <= 20,
            "expected at most 20 results, got {result_count}"
        );
    }

    // ── format_results unit ───────────────────────────────────────────────────

    #[test]
    fn format_results_empty_returns_no_match_message() {
        let r = format_results(&[]);
        assert!(!r.is_error);
        assert!(r.for_llm.contains("No matching notes"));
    }

    #[test]
    fn format_results_single_entry() {
        let rows = vec![(
            "note.md".to_string(),
            "...some **keyword** here...".to_string(),
        )];
        let r = format_results(&rows);
        assert!(!r.is_error);
        assert!(r.for_llm.contains("Found 1 result"));
        assert!(r.for_llm.contains("note.md"));
        assert!(r.for_llm.contains("**keyword**"));
    }

    #[test]
    fn format_results_multiple_entries_numbered() {
        let rows = vec![
            ("a.md".to_string(), "snip a".to_string()),
            ("b.md".to_string(), "snip b".to_string()),
            ("c.md".to_string(), "snip c".to_string()),
        ];
        let r = format_results(&rows);
        assert!(r.for_llm.contains("Found 3 result"));
        assert!(r.for_llm.contains("1. a.md"));
        assert!(r.for_llm.contains("2. b.md"));
        assert!(r.for_llm.contains("3. c.md"));
    }

    // ── Unicode query ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn unicode_query_does_not_crash() {
        let (_tmp, db) = temp_db();
        index(&db, "日記.md", "今日はベンチプレス100kgを達成した。");

        let tool = SearchVaultTool::new(Arc::clone(&db));
        let res = tool
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "query": "ベンチプレス" }),
            )
            .await;
        // Must not crash. May or may not match depending on FTS5 tokenizer.
        assert!(!res.is_error, "unexpected error: {}", res.for_llm);
    }
}
