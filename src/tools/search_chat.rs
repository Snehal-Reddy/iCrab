//! `search_chat` tool: BM25 keyword search over local chat history.
//!
//! Executes against the `chat_fts` FTS5 table (backed by `chat_history`),
//! returning ranked (chat_id, role, snippet) triples.  Deliberately separate
//! from `search_vault` so the agent can recall past conversations without
//! touching the vault index.

use std::sync::Arc;

use serde_json::Value;

use crate::memory::db::{BrainDb, DbError};
use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

const DEFAULT_LIMIT: usize = 5;

pub struct SearchChatTool {
    db: Arc<BrainDb>,
}

impl SearchChatTool {
    pub fn new(db: Arc<BrainDb>) -> Self {
        Self { db }
    }
}

impl Tool for SearchChatTool {
    fn name(&self) -> &str {
        "search_chat"
    }

    fn description(&self) -> &str {
        "Search past chat history for a keyword query. \
         Returns BM25-ranked results showing which conversation and message role \
         matched, plus a snippet of the matching text. \
         Use this to recall specific facts or topics discussed in past sessions."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to search for in past chat messages. \
                        Supports multi-word queries, phrases (\"bench press\"), \
                        prefix wildcards (squat*), and boolean operators (OR, NOT)."
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

            let result =
                tokio::task::spawn_blocking(move || chat_search_with_fallback(&db, &query, limit))
                    .await;

            match result {
                Ok(Ok(rows)) => format_results(&rows),
                Ok(Err(e)) => ToolResult::error(format!("search failed: {e}")),
                Err(e) => ToolResult::error(format!("search task error: {e}")),
            }
        })
    }
}

fn chat_search_with_fallback(
    db: &BrainDb,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>, DbError> {
    match db.chat_fts_search(query, limit) {
        Ok(rows) => Ok(rows),
        Err(_) => {
            let safe: String = query
                .split_whitespace()
                .filter(|w| !w.is_empty())
                .map(|w| format!("\"{}\"", w.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" OR ");

            if safe.is_empty() {
                Ok(Vec::new())
            } else {
                db.chat_fts_search(&safe, limit)
            }
        }
    }
}

fn format_results(rows: &[(String, String, String)]) -> ToolResult {
    if rows.is_empty() {
        return ToolResult::ok("No matching messages found in chat history.");
    }

    let mut out = format!("Found {} result(s) in chat history:\n", rows.len());
    for (i, (chat_id, role, snippet)) in rows.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. [{}] {}\n   {}\n",
            i + 1,
            role,
            chat_id,
            snippet
        ));
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

    use crate::memory::db::{BrainDb, StoredMessage};
    use crate::tools::context::ToolCtx;
    use crate::tools::registry::Tool;

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

    fn seed(db: &BrainDb, chat_id: &str, role: &str, content: &str) {
        db.save_session(
            chat_id,
            &[StoredMessage {
                role: role.into(),
                content: content.into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            "",
        )
        .unwrap();
    }

    #[test]
    fn tool_name() {
        let (_tmp, db) = temp_db();
        assert_eq!(SearchChatTool::new(db).name(), "search_chat");
    }

    #[test]
    fn tool_parameters_require_query() {
        let (_tmp, db) = temp_db();
        let params = SearchChatTool::new(db).parameters();
        assert_eq!(params["required"][0], "query");
    }

    #[tokio::test]
    async fn missing_query_returns_error() {
        let (_tmp, db) = temp_db();
        let res = SearchChatTool::new(db)
            .execute(&dummy_ctx(), &serde_json::json!({}))
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn empty_vault_returns_no_match() {
        let (_tmp, db) = temp_db();
        let res = SearchChatTool::new(db)
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "squats" }))
            .await;
        assert!(!res.is_error);
        assert!(res.for_llm.contains("No matching"));
    }

    #[tokio::test]
    async fn finds_saved_message() {
        let (_tmp, db) = temp_db();
        seed(&db, "c1", "user", "I did squats today");

        let res = SearchChatTool::new(Arc::clone(&db))
            .execute(&dummy_ctx(), &serde_json::json!({ "query": "squats" }))
            .await;
        assert!(!res.is_error, "{}", res.for_llm);
        assert!(res.for_llm.contains("c1"));
        assert!(res.for_llm.contains("user"));
    }

    #[tokio::test]
    async fn invalid_fts5_query_falls_back_gracefully() {
        let (_tmp, db) = temp_db();
        seed(&db, "c1", "user", "hello world");

        let res = SearchChatTool::new(Arc::clone(&db))
            .execute(
                &dummy_ctx(),
                &serde_json::json!({ "query": "AND OR NOT" }),
            )
            .await;
        assert!(!res.is_error, "{}", res.for_llm);
    }

    #[test]
    fn format_results_empty() {
        let r = format_results(&[]);
        assert!(!r.is_error);
        assert!(r.for_llm.contains("No matching"));
    }

    #[test]
    fn format_results_single() {
        let rows = vec![(
            "chat123".to_string(),
            "user".to_string(),
            "...did **squats** today...".to_string(),
        )];
        let r = format_results(&rows);
        assert!(r.for_llm.contains("1 result"));
        assert!(r.for_llm.contains("chat123"));
        assert!(r.for_llm.contains("user"));
    }
}
