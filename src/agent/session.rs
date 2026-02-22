//! Per-chat session: history and summary persisted in SQLite via BrainDb.
//!
//! Replaces the old `sessions/<chat_id>.json` approach. The `Session` struct
//! keeps an in-memory `Vec<Message>` + summary string, loading from and saving
//! to the `chat_history` / `chat_summary` tables in `BrainDb`.

use std::sync::Arc;

use crate::llm::{Message, Role, ToolCall};
use crate::memory::db::{BrainDb, DbError, StoredMessage};

const MAX_HISTORY: usize = 50;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SessionError {
    Db(String),
    Serialize(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Db(s) => write!(f, "session db: {}", s),
            SessionError::Serialize(s) => write!(f, "session serialize: {}", s),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<DbError> for SessionError {
    fn from(e: DbError) -> Self {
        SessionError::Db(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// In-memory session: history and optional summary. Cap history at MAX_HISTORY.
/// Backed by the `chat_history` and `chat_summary` tables in `BrainDb`.
#[derive(Debug, Clone)]
pub struct Session {
    history: Vec<Message>,
    summary: String,
    chat_id: String,
    db: Arc<BrainDb>,
}

impl Session {
    /// Load session from the database; missing chat_id → empty session.
    pub async fn load(db: Arc<BrainDb>, chat_id: &str) -> Result<Self, SessionError> {
        let chat_id = chat_id.to_string();
        let db_clone = Arc::clone(&db);
        let chat_id_clone = chat_id.clone();

        let (stored, summary) =
            tokio::task::spawn_blocking(move || db_clone.load_session(&chat_id_clone))
                .await
                .map_err(|e| SessionError::Db(format!("spawn_blocking: {e}")))?
                .map_err(SessionError::from)?;

        let history = stored
            .into_iter()
            .map(stored_to_message)
            .collect::<Result<Vec<_>, _>>()?;

        let mut session = Self {
            history,
            summary,
            chat_id,
            db,
        };
        // Enforce cap in case the DB somehow has more than MAX_HISTORY rows.
        session.cap_history();
        Ok(session)
    }

    /// Persist the session (history + summary) to the database.
    pub async fn save(&self) -> Result<(), SessionError> {
        let stored: Vec<StoredMessage> = self
            .history
            .iter()
            .map(message_to_stored)
            .collect::<Result<Vec<_>, _>>()?;

        let chat_id = self.chat_id.clone();
        let summary = self.summary.clone();
        let db = Arc::clone(&self.db);

        tokio::task::spawn_blocking(move || db.save_session(&chat_id, &stored, &summary))
            .await
            .map_err(|e| SessionError::Db(format!("spawn_blocking: {e}")))?
            .map_err(SessionError::from)
    }

    // -----------------------------------------------------------------------
    // Mutation helpers
    // -----------------------------------------------------------------------

    pub fn add_user_message(&mut self, content: &str) {
        self.history.push(Message {
            role: Role::User,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        });
        self.cap_history();
    }

    pub fn add_assistant_message(&mut self, content: &str, tool_calls: Option<Vec<ToolCall>>) {
        self.history.push(Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls,
        });
        self.cap_history();
    }

    pub fn add_tool_message(&mut self, tool_call_id: &str, content: &str) {
        self.history.push(Message {
            role: Role::Tool,
            content: content.to_string(),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_calls: None,
        });
        self.cap_history();
    }

    fn cap_history(&mut self) {
        if self.history.len() > MAX_HISTORY {
            self.history.drain(..self.history.len() - MAX_HISTORY);
        }
    }

    // -----------------------------------------------------------------------
    // Read-only accessors
    // -----------------------------------------------------------------------

    #[inline]
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    #[inline]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn set_summary(&mut self, s: String) {
        self.summary = s;
    }

    /// Truncate history to the last `keep` messages. No-op if already shorter.
    pub fn truncate_history(&mut self, keep: usize) {
        if self.history.len() > keep {
            let start = self.history.len() - keep;
            self.history.drain(..start);
        }
    }
}

// ---------------------------------------------------------------------------
// Message ↔ StoredMessage conversions
// ---------------------------------------------------------------------------

fn role_to_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn str_to_role(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

fn message_to_stored(msg: &Message) -> Result<StoredMessage, SessionError> {
    let tool_calls = msg
        .tool_calls
        .as_ref()
        .map(|tc| serde_json::to_string(tc).map_err(|e| SessionError::Serialize(e.to_string())))
        .transpose()?;

    Ok(StoredMessage {
        role: role_to_str(&msg.role).to_string(),
        content: msg.content.clone(),
        tool_call_id: msg.tool_call_id.clone(),
        tool_calls,
    })
}

fn stored_to_message(stored: StoredMessage) -> Result<Message, SessionError> {
    let tool_calls = stored
        .tool_calls
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            serde_json::from_str::<Vec<ToolCall>>(s)
                .map_err(|e| SessionError::Serialize(e.to_string()))
        })
        .transpose()?;

    Ok(Message {
        role: str_to_role(&stored.role),
        content: stored.content,
        tool_call_id: stored.tool_call_id,
        tool_calls,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_db() -> (TempDir, Arc<BrainDb>) {
        let tmp = TempDir::new().unwrap();
        let db = Arc::new(BrainDb::open(tmp.path()).unwrap());
        (tmp, db)
    }

    // ── Load missing → empty session ─────────────────────────────────────────

    #[tokio::test]
    async fn session_load_missing_returns_empty() {
        let (_tmp, db) = temp_db();
        let s = Session::load(db, "nonexistent").await.unwrap();
        assert!(s.history().is_empty());
        assert!(s.summary().is_empty());
    }

    // ── Save & load roundtrip ─────────────────────────────────────────────────

    #[tokio::test]
    async fn session_save_load_roundtrip() {
        let (_tmp, db) = temp_db();
        let mut session = Session::load(Arc::clone(&db), "chat1").await.unwrap();
        session.add_user_message("Hi");
        session.add_assistant_message("Hello!", None);
        session.set_summary("brief".to_string());
        session.save().await.unwrap();

        let loaded = Session::load(Arc::clone(&db), "chat1").await.unwrap();
        assert_eq!(loaded.history().len(), 2);
        assert_eq!(loaded.history()[0].content, "Hi");
        assert_eq!(loaded.history()[1].content, "Hello!");
        assert_eq!(loaded.summary(), "brief");
    }

    // ── Overwrite on second save ──────────────────────────────────────────────

    #[tokio::test]
    async fn session_save_overwrites() {
        let (_tmp, db) = temp_db();

        // First save
        let mut s1 = Session::load(Arc::clone(&db), "c").await.unwrap();
        s1.add_user_message("First");
        s1.save().await.unwrap();

        // Second save with more messages
        let mut s2 = Session::load(Arc::clone(&db), "c").await.unwrap();
        assert_eq!(s2.history().len(), 1);
        s2.add_assistant_message("OK", None);
        s2.add_user_message("Second");
        s2.set_summary("updated summary".to_string());
        s2.save().await.unwrap();

        let loaded = Session::load(Arc::clone(&db), "c").await.unwrap();
        assert_eq!(loaded.history().len(), 3);
        assert_eq!(loaded.summary(), "updated summary");
    }

    // ── History cap ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn session_add_messages_caps_history() {
        let (_tmp, db) = temp_db();
        let mut session = Session::load(Arc::clone(&db), "cap").await.unwrap();
        for i in 0..55 {
            session.add_user_message(&format!("msg {}", i));
        }
        assert_eq!(session.history().len(), MAX_HISTORY);
        assert_eq!(session.history().first().unwrap().content, "msg 5");
    }

    // ── Truncate history ──────────────────────────────────────────────────────

    #[test]
    fn session_truncate_history() {
        let tmp = TempDir::new().unwrap();
        let db = Arc::new(BrainDb::open(tmp.path()).unwrap());
        let mut session = Session {
            history: Vec::new(),
            summary: String::new(),
            chat_id: "truncate".to_string(),
            db,
        };

        for i in 0..10 {
            session.add_user_message(&format!("msg {}", i));
        }
        assert_eq!(session.history().len(), 10);

        session.truncate_history(4);
        assert_eq!(session.history().len(), 4);
        assert_eq!(session.history()[0].content, "msg 6");
        assert_eq!(session.history()[3].content, "msg 9");

        // Truncate when already shorter — no-op
        session.truncate_history(10);
        assert_eq!(session.history().len(), 4);
    }

    // ── Tool messages roundtrip ───────────────────────────────────────────────

    #[tokio::test]
    async fn session_tool_message_roundtrip() {
        let (_tmp, db) = temp_db();
        let mut session = Session::load(Arc::clone(&db), "tool").await.unwrap();
        session.add_tool_message("call_1", "file contents");
        session.save().await.unwrap();

        let loaded = Session::load(Arc::clone(&db), "tool").await.unwrap();
        assert_eq!(loaded.history().len(), 1);
        assert_eq!(loaded.history()[0].role, Role::Tool);
        assert_eq!(loaded.history()[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(loaded.history()[0].content, "file contents");
    }

    // ── Sessions isolated by chat_id ──────────────────────────────────────────

    #[tokio::test]
    async fn sessions_isolated_by_chat_id() {
        let (_tmp, db) = temp_db();

        let mut a = Session::load(Arc::clone(&db), "A").await.unwrap();
        a.add_user_message("from A");
        a.save().await.unwrap();

        let mut b = Session::load(Arc::clone(&db), "B").await.unwrap();
        b.add_user_message("from B");
        b.save().await.unwrap();

        let la = Session::load(Arc::clone(&db), "A").await.unwrap();
        let lb = Session::load(Arc::clone(&db), "B").await.unwrap();
        assert_eq!(la.history()[0].content, "from A");
        assert_eq!(lb.history()[0].content, "from B");
    }

    // ── Persist across DB reopen ──────────────────────────────────────────────

    #[tokio::test]
    async fn session_persists_across_db_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let db = Arc::new(BrainDb::open(tmp.path()).unwrap());
            let mut s = Session::load(Arc::clone(&db), "persist").await.unwrap();
            s.add_user_message("survive restart");
            s.set_summary("persisted summary".to_string());
            s.save().await.unwrap();
        }
        // Reopen DB
        let db2 = Arc::new(BrainDb::open(tmp.path()).unwrap());
        let loaded = Session::load(db2, "persist").await.unwrap();
        assert_eq!(loaded.history().len(), 1);
        assert_eq!(loaded.history()[0].content, "survive restart");
        assert_eq!(loaded.summary(), "persisted summary");
    }

    // ── Unicode ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn session_unicode_roundtrip() {
        let (_tmp, db) = temp_db();
        let mut s = Session::load(Arc::clone(&db), "uni").await.unwrap();
        s.add_user_message("こんにちは 🚀");
        s.set_summary("日本語".to_string());
        s.save().await.unwrap();

        let loaded = Session::load(Arc::clone(&db), "uni").await.unwrap();
        assert_eq!(loaded.history()[0].content, "こんにちは 🚀");
        assert_eq!(loaded.summary(), "日本語");
    }
}
