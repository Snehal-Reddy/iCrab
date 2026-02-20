//! SQLite brain database: schema init, chat history persistence, vault index for FTS5.
//!
//! Lives at `workspace/.icrab/brain.db` (Git-ignored).
//!
//! Tables:
//! - `chat_history`  — persistent chat messages per session (replaces sessions/*.json)
//! - `chat_summary`  — per-session LLM-generated summary string
//! - `vault_index`   — mirrors Obsidian Markdown files
//! - `vault_fts`     — FTS5 virtual table with BM25 scoring

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};

use crate::workspace;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "db: {}", self.0)
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// BrainDb
// ---------------------------------------------------------------------------

/// Persistent SQLite brain for iCrab.
///
/// Uses a single `Mutex<Connection>` — safe to share across async tasks via
/// `Arc<BrainDb>` since all operations take the lock synchronously.
/// (rusqlite `Connection` is `Send` but not `Sync`.)
pub struct BrainDb {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for BrainDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrainDb").finish_non_exhaustive()
    }
}

impl BrainDb {
    /// Open (or create) the brain database at `workspace/.icrab/brain.db`.
    /// Creates the `.icrab/` directory if it does not exist.
    pub fn open(workspace: &Path) -> Result<Self, DbError> {
        let db_path = workspace::brain_db_path(workspace);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DbError(format!("create_dir_all: {e}")))?;
        }

        let conn = Connection::open(&db_path)
            .map_err(|e| DbError(format!("open {}: {e}", db_path.display())))?;

        // iSH-optimised PRAGMAs:
        // WAL + NORMAL sync: durable and 2× faster writes on constrained flash.
        // mmap 8 MiB: let OS page-cache serve hot reads without extra copies.
        // temp_store MEMORY: temp tables never hit slow iSH storage.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA mmap_size    = 8388608;
             PRAGMA temp_store   = MEMORY;",
        )?;

        Self::init_schema(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // -----------------------------------------------------------------------
    // Schema
    // -----------------------------------------------------------------------

    fn init_schema(conn: &Connection) -> Result<(), DbError> {
        conn.execute_batch(
            "-- ── Chat history ─────────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS chat_history (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                chat_id      TEXT    NOT NULL,
                role         TEXT    NOT NULL,
                content      TEXT    NOT NULL,
                tool_call_id TEXT,
                tool_calls   TEXT,
                timestamp    DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_chat_history_chat_id
                ON chat_history(chat_id, id);

            -- ── Chat summaries ──────────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS chat_summary (
                chat_id TEXT PRIMARY KEY,
                summary TEXT NOT NULL DEFAULT ''
            );

            -- ── Vault index  ──────────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS vault_index (
                filepath      TEXT    PRIMARY KEY,
                content       TEXT,
                last_modified INTEGER
            );

            -- ── Vault FTS5  ──────────────────────────────────────────────────────
            CREATE VIRTUAL TABLE IF NOT EXISTS vault_fts USING fts5(
                filepath, content,
                content=vault_index,
                content_rowid=rowid
            );

            -- Triggers: keep vault_fts in sync with vault_index
            CREATE TRIGGER IF NOT EXISTS vault_index_ai
                AFTER INSERT ON vault_index BEGIN
                    INSERT INTO vault_fts(rowid, filepath, content)
                    VALUES (new.rowid, new.filepath, new.content);
                END;
            CREATE TRIGGER IF NOT EXISTS vault_index_ad
                AFTER DELETE ON vault_index BEGIN
                    INSERT INTO vault_fts(vault_fts, rowid, filepath, content)
                    VALUES ('delete', old.rowid, old.filepath, old.content);
                END;
            CREATE TRIGGER IF NOT EXISTS vault_index_au
                AFTER UPDATE ON vault_index BEGIN
                    INSERT INTO vault_fts(vault_fts, rowid, filepath, content)
                    VALUES ('delete', old.rowid, old.filepath, old.content);
                    INSERT INTO vault_fts(rowid, filepath, content)
                    VALUES (new.rowid, new.filepath, new.content);
                END;",
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Chat history operations
    // -----------------------------------------------------------------------

    /// Persist the entire session (messages + summary) atomically.
    ///
    /// Clears existing rows for `chat_id`, then inserts all `messages`, then
    /// upserts the `summary`. Wrapped in a transaction for atomicity.
    pub fn save_session(
        &self,
        chat_id: &str,
        messages: &[StoredMessage],
        summary: &str,
    ) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        conn.execute_batch("BEGIN;")?;

        conn.execute("DELETE FROM chat_history WHERE chat_id = ?1", params![chat_id])?;

        for msg in messages {
            conn.execute(
                "INSERT INTO chat_history (chat_id, role, content, tool_call_id, tool_calls)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    chat_id,
                    msg.role,
                    msg.content,
                    msg.tool_call_id,
                    msg.tool_calls,
                ],
            )?;
        }

        conn.execute(
            "INSERT OR REPLACE INTO chat_summary (chat_id, summary) VALUES (?1, ?2)",
            params![chat_id, summary],
        )?;

        conn.execute_batch("COMMIT;")?;
        Ok(())
    }

    /// Load all messages and the summary for `chat_id`.
    /// Returns `(messages, summary)`. Missing session → empty vec and empty string.
    pub fn load_session(
        &self,
        chat_id: &str,
    ) -> Result<(Vec<StoredMessage>, String), DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        let mut stmt = conn.prepare(
            "SELECT role, content, tool_call_id, tool_calls
             FROM chat_history
             WHERE chat_id = ?1
             ORDER BY id ASC",
        )?;

        let messages: Vec<StoredMessage> = stmt
            .query_map(params![chat_id], |row| {
                Ok(StoredMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    tool_call_id: row.get(2)?,
                    tool_calls: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;

        let summary: String = conn
            .query_row(
                "SELECT summary FROM chat_summary WHERE chat_id = ?1",
                params![chat_id],
                |row| row.get(0),
            )
            .unwrap_or_default();

        Ok((messages, summary))
    }

    /// Health check: execute a trivial query.
    pub fn health_check(&self) -> bool {
        self.conn
            .lock()
            .map(|c| c.execute_batch("SELECT 1").is_ok())
            .unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // Vault index operations
    // -----------------------------------------------------------------------

    /// Upsert a vault file entry. The triggers in the schema keep `vault_fts`
    /// in sync automatically on every INSERT OR REPLACE.
    pub fn upsert_vault_entry(
        &self,
        filepath: &str,
        content: &str,
        last_modified: i64,
    ) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        conn.execute(
            "INSERT OR REPLACE INTO vault_index (filepath, content, last_modified)
             VALUES (?1, ?2, ?3)",
            params![filepath, content, last_modified],
        )?;
        Ok(())
    }

    /// Return the stored `last_modified` timestamp for a vault file, or `None`
    /// if the file has not been indexed yet.
    pub fn get_vault_last_modified(&self, filepath: &str) -> Result<Option<i64>, DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        match conn.query_row(
            "SELECT last_modified FROM vault_index WHERE filepath = ?1",
            params![filepath],
            |row| row.get(0),
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DbError(e.to_string())),
        }
    }

    /// Delete all `vault_index` rows whose filepath is **not** present in
    /// `known_paths`. Returns the number of rows deleted.
    ///
    /// Holds a single lock for the entire operation (no nested locks).
    pub fn delete_vault_stale(
        &self,
        known_paths: &std::collections::HashSet<String>,
    ) -> Result<usize, DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        // Collect all stored filepaths while holding the lock.
        let stored: Vec<String> = {
            let mut stmt = conn.prepare("SELECT filepath FROM vault_index")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };

        let mut deleted = 0usize;
        for fp in stored {
            if !known_paths.contains(&fp) {
                deleted +=
                    conn.execute("DELETE FROM vault_index WHERE filepath = ?1", params![fp])?;
            }
        }
        Ok(deleted)
    }

    /// Return the filepaths of all entries currently in `vault_index`.
    pub fn list_vault_filepaths(&self) -> Result<Vec<String>, DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;
        let mut stmt = conn.prepare("SELECT filepath FROM vault_index ORDER BY filepath ASC")?;
        let paths: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(paths)
    }

    // -----------------------------------------------------------------------
    // Vault FTS5 queries
    // -----------------------------------------------------------------------

    /// Count documents whose `vault_fts` entry matches `fts_query` (FTS5
    /// syntax, e.g. `"\"squats\""` for exact-phrase match).
    ///
    /// Useful for diagnostics, testing, and the search tool.
    pub fn vault_fts_count(&self, fts_query: &str) -> Result<usize, DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vault_fts WHERE vault_fts MATCH ?1",
                params![fts_query],
                |row| row.get::<_, i64>(0),
            )
            .map_err(DbError::from)?;

        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(count as usize)
    }

    /// Return the stored content of a single vault file, or `None` if not indexed.
    pub fn get_vault_content(&self, filepath: &str) -> Result<Option<String>, DbError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        match conn.query_row(
            "SELECT content FROM vault_index WHERE filepath = ?1",
            params![filepath],
            |row| row.get::<_, String>(0),
        ) {
            Ok(c) => Ok(Some(c)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DbError(e.to_string())),
        }
    }

    /// Return a BM25-ranked list of `(filepath, snippet)` pairs for `fts_query`.
    ///
    /// `snippet_col` is the FTS5 column index for `snippet()` (-1 = best).
    /// Returns at most `limit` results.  This is the foundation for the
    pub fn vault_fts_search(
        &self,
        fts_query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, DbError> {
        if fts_query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| DbError(format!("lock: {e}")))?;

        #[allow(clippy::cast_possible_wrap)]
        let limit_i64 = limit as i64;

        let mut stmt = conn.prepare(
            "SELECT filepath, snippet(vault_fts, -1, '**', '**', '...', 10) AS snip
             FROM vault_fts
             WHERE vault_fts MATCH ?1
             ORDER BY bm25(vault_fts)
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![fts_query, limit_i64], |row| {
            let fp: String = row.get(0)?;
            let sn: String = row.get(1)?;
            Ok((fp, sn))
        })?;

        let results: Vec<(String, String)> = rows.collect::<Result<_, _>>()?;
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// StoredMessage (DB row ↔ Vec<Message> bridge)
// ---------------------------------------------------------------------------

/// A flat representation of a chat message as stored in `chat_history`.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    /// `tool_call_id` for `Role::Tool` messages.
    pub tool_call_id: Option<String>,
    /// JSON-serialised `Vec<ToolCall>` for `Role::Assistant` messages that
    /// triggered tool calls (usually `None` for final assistant replies).
    pub tool_calls: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_db() -> (TempDir, BrainDb) {
        let tmp = TempDir::new().unwrap();
        let db = BrainDb::open(tmp.path()).unwrap();
        (tmp, db)
    }

    // ── Open & health ────────────────────────────────────────────────────────

    #[test]
    fn open_creates_db_file() {
        let tmp = TempDir::new().unwrap();
        BrainDb::open(tmp.path()).unwrap();
        assert!(workspace::brain_db_path(tmp.path()).exists());
    }

    #[test]
    fn health_check_passes() {
        let (_tmp, db) = temp_db();
        assert!(db.health_check());
    }

    #[test]
    fn open_idempotent_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let db = BrainDb::open(tmp.path()).unwrap();
            assert!(db.health_check());
        }
        // Reopen — schema init must be safe with IF NOT EXISTS
        let db2 = BrainDb::open(tmp.path()).unwrap();
        assert!(db2.health_check());
    }

    // ── chat_history: empty session ──────────────────────────────────────────

    #[test]
    fn load_session_missing_returns_empty() {
        let (_tmp, db) = temp_db();
        let (msgs, summary) = db.load_session("nonexistent").unwrap();
        assert!(msgs.is_empty());
        assert!(summary.is_empty());
    }

    // ── chat_history: save & load roundtrip ─────────────────────────────────

    #[test]
    fn save_load_roundtrip() {
        let (_tmp, db) = temp_db();
        let messages = vec![
            StoredMessage {
                role: "user".into(),
                content: "Hello".into(),
                tool_call_id: None,
                tool_calls: None,
            },
            StoredMessage {
                role: "assistant".into(),
                content: "Hi there!".into(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        db.save_session("chat1", &messages, "brief summary").unwrap();

        let (loaded, summary) = db.load_session("chat1").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, "user");
        assert_eq!(loaded[0].content, "Hello");
        assert_eq!(loaded[1].role, "assistant");
        assert_eq!(loaded[1].content, "Hi there!");
        assert_eq!(summary, "brief summary");
    }

    // ── chat_history: overwrite on second save ───────────────────────────────

    #[test]
    fn save_overwrites_previous() {
        let (_tmp, db) = temp_db();
        let msgs1 = vec![StoredMessage {
            role: "user".into(),
            content: "First".into(),
            tool_call_id: None,
            tool_calls: None,
        }];
        db.save_session("c", &msgs1, "sum1").unwrap();

        let msgs2 = vec![
            StoredMessage {
                role: "user".into(),
                content: "First".into(),
                tool_call_id: None,
                tool_calls: None,
            },
            StoredMessage {
                role: "assistant".into(),
                content: "OK".into(),
                tool_call_id: None,
                tool_calls: None,
            },
            StoredMessage {
                role: "user".into(),
                content: "Second".into(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        db.save_session("c", &msgs2, "sum2").unwrap();

        let (loaded, summary) = db.load_session("c").unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(summary, "sum2");
    }

    // ── chat_history: sessions are isolated by chat_id ──────────────────────

    #[test]
    fn sessions_isolated_by_chat_id() {
        let (_tmp, db) = temp_db();
        let a = vec![StoredMessage {
            role: "user".into(),
            content: "from A".into(),
            tool_call_id: None,
            tool_calls: None,
        }];
        let b = vec![StoredMessage {
            role: "user".into(),
            content: "from B".into(),
            tool_call_id: None,
            tool_calls: None,
        }];
        db.save_session("A", &a, "").unwrap();
        db.save_session("B", &b, "").unwrap();

        let (la, _) = db.load_session("A").unwrap();
        let (lb, _) = db.load_session("B").unwrap();
        assert_eq!(la[0].content, "from A");
        assert_eq!(lb[0].content, "from B");
    }

    // ── chat_history: tool message fields roundtrip ──────────────────────────

    #[test]
    fn tool_message_fields_roundtrip() {
        let (_tmp, db) = temp_db();
        let messages = vec![
            StoredMessage {
                role: "assistant".into(),
                content: "".into(),
                tool_call_id: None,
                tool_calls: Some(r#"[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{}"}}]"#.into()),
            },
            StoredMessage {
                role: "tool".into(),
                content: "file contents".into(),
                tool_call_id: Some("c1".into()),
                tool_calls: None,
            },
        ];
        db.save_session("tool_chat", &messages, "").unwrap();

        let (loaded, _) = db.load_session("tool_chat").unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded[0].tool_calls.is_some());
        assert_eq!(loaded[1].tool_call_id.as_deref(), Some("c1"));
    }

    // ── chat_summary: empty summary upserts correctly ────────────────────────

    #[test]
    fn empty_summary_upserts() {
        let (_tmp, db) = temp_db();
        db.save_session("s", &[], "").unwrap();
        let (_, summary) = db.load_session("s").unwrap();
        assert_eq!(summary, "");
    }

    #[test]
    fn summary_updated_on_second_save() {
        let (_tmp, db) = temp_db();
        db.save_session("s", &[], "old summary").unwrap();
        db.save_session("s", &[], "new summary").unwrap();
        let (_, summary) = db.load_session("s").unwrap();
        assert_eq!(summary, "new summary");
    }

    // ── Schema: tables exist ─────────────────────────────────────────────────

    #[test]
    fn schema_has_all_tables() {
        let (_tmp, db) = temp_db();
        let conn = db.conn.lock().unwrap();
        for table in &["chat_history", "chat_summary", "vault_index"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table '{}' should exist", table);
        }
    }

    #[test]
    fn schema_has_vault_fts_virtual_table() {
        let (_tmp, db) = temp_db();
        let conn = db.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='vault_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "vault_fts virtual table should exist");
    }

    // ── Vault index: BrainDb operations ─────────────────────────────────────

    #[test]
    fn upsert_vault_entry_and_get_mtime() {
        let (_tmp, db) = temp_db();
        db.upsert_vault_entry("Daily log/2026-02-20.md", "Ran 5km today.", 1_708_384_000)
            .unwrap();
        let mtime = db
            .get_vault_last_modified("Daily log/2026-02-20.md")
            .unwrap();
        assert_eq!(mtime, Some(1_708_384_000));
    }

    #[test]
    fn upsert_vault_entry_replaces_existing() {
        let (_tmp, db) = temp_db();
        db.upsert_vault_entry("note.md", "old content", 100).unwrap();
        db.upsert_vault_entry("note.md", "new content", 200).unwrap();

        let mtime = db.get_vault_last_modified("note.md").unwrap();
        assert_eq!(mtime, Some(200));

        // FTS5 should see new content, not old
        let conn = db.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vault_fts WHERE vault_fts MATCH '\"new\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn get_vault_last_modified_missing() {
        let (_tmp, db) = temp_db();
        let mtime = db.get_vault_last_modified("not_indexed.md").unwrap();
        assert_eq!(mtime, None);
    }

    #[test]
    fn list_vault_filepaths_empty() {
        let (_tmp, db) = temp_db();
        let paths = db.list_vault_filepaths().unwrap();
        assert!(paths.is_empty());
    }

    #[test]
    fn list_vault_filepaths_sorted() {
        let (_tmp, db) = temp_db();
        db.upsert_vault_entry("z.md", "z", 0).unwrap();
        db.upsert_vault_entry("a.md", "a", 0).unwrap();
        db.upsert_vault_entry("m.md", "m", 0).unwrap();

        let paths = db.list_vault_filepaths().unwrap();
        assert_eq!(paths, vec!["a.md", "m.md", "z.md"]);
    }

    #[test]
    fn delete_vault_stale_removes_unlisted() {
        use std::collections::HashSet;
        let (_tmp, db) = temp_db();
        db.upsert_vault_entry("keep.md", "kept", 1).unwrap();
        db.upsert_vault_entry("stale1.md", "gone1", 2).unwrap();
        db.upsert_vault_entry("stale2.md", "gone2", 3).unwrap();

        let known: HashSet<String> = vec!["keep.md".to_string()].into_iter().collect();
        let deleted = db.delete_vault_stale(&known).unwrap();
        assert_eq!(deleted, 2);

        let paths = db.list_vault_filepaths().unwrap();
        assert_eq!(paths, vec!["keep.md"]);
    }

    #[test]
    fn delete_vault_stale_empty_known_deletes_all() {
        use std::collections::HashSet;
        let (_tmp, db) = temp_db();
        db.upsert_vault_entry("a.md", "a", 1).unwrap();
        db.upsert_vault_entry("b.md", "b", 2).unwrap();

        let known: HashSet<String> = HashSet::new();
        let deleted = db.delete_vault_stale(&known).unwrap();
        assert_eq!(deleted, 2);
        assert!(db.list_vault_filepaths().unwrap().is_empty());
    }

    #[test]
    fn delete_vault_stale_all_known_deletes_none() {
        use std::collections::HashSet;
        let (_tmp, db) = temp_db();
        db.upsert_vault_entry("a.md", "a", 1).unwrap();
        db.upsert_vault_entry("b.md", "b", 2).unwrap();

        let known: HashSet<String> = vec!["a.md".to_string(), "b.md".to_string()]
            .into_iter()
            .collect();
        let deleted = db.delete_vault_stale(&known).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(db.list_vault_filepaths().unwrap().len(), 2);
    }

    // ── Vault index: basic insert & fts5 roundtrip ───────────────────────────

    #[test]
    fn vault_index_insert_and_fts5_search() {
        let (_tmp, db) = temp_db();
        let conn = db.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO vault_index (filepath, content, last_modified)
             VALUES (?1, ?2, ?3)",
            params!["Daily log/2026-02-20.md", "Did a run today, felt great.", 0i64],
        )
        .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vault_fts WHERE vault_fts MATCH '\"run\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "FTS5 should find the inserted document");
    }

    #[test]
    fn vault_index_fts5_delete_trigger() {
        let (_tmp, db) = temp_db();
        let conn = db.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO vault_index (filepath, content, last_modified) VALUES (?1, ?2, 0)",
            params!["note.md", "unique_searchterm_xyz"],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM vault_index WHERE filepath = ?1",
            params!["note.md"],
        )
        .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vault_fts WHERE vault_fts MATCH '\"unique_searchterm_xyz\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "Deleted entry should not appear in FTS5");
    }

    // ── Persistence: data survives reopen ────────────────────────────────────

    #[test]
    fn data_persists_across_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let db = BrainDb::open(tmp.path()).unwrap();
            db.save_session(
                "persist",
                &[StoredMessage {
                    role: "user".into(),
                    content: "survive restarts".into(),
                    tool_call_id: None,
                    tool_calls: None,
                }],
                "persisted summary",
            )
            .unwrap();
        }
        let db2 = BrainDb::open(tmp.path()).unwrap();
        let (msgs, summary) = db2.load_session("persist").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "survive restarts");
        assert_eq!(summary, "persisted summary");
    }

    // ── Edge: unicode and special characters ─────────────────────────────────

    #[test]
    fn unicode_content_roundtrip() {
        let (_tmp, db) = temp_db();
        db.save_session(
            "unicode",
            &[StoredMessage {
                role: "user".into(),
                content: "こんにちは 🚀 Ñoño".into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            "日本語サマリー",
        )
        .unwrap();
        let (msgs, summary) = db.load_session("unicode").unwrap();
        assert_eq!(msgs[0].content, "こんにちは 🚀 Ñoño");
        assert_eq!(summary, "日本語サマリー");
    }

    #[test]
    fn message_ordering_preserved() {
        let (_tmp, db) = temp_db();
        let messages: Vec<StoredMessage> = (0..10)
            .map(|i| StoredMessage {
                role: "user".into(),
                content: format!("message {i}"),
                tool_call_id: None,
                tool_calls: None,
            })
            .collect();
        db.save_session("order", &messages, "").unwrap();
        let (loaded, _) = db.load_session("order").unwrap();
        for (i, msg) in loaded.iter().enumerate() {
            assert_eq!(msg.content, format!("message {i}"));
        }
    }
}
