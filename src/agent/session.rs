//! Per-chat session: last N messages, optional summary; persist under workspace/sessions/.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::llm::{Message, Role, ToolCall};
use crate::workspace;

const MAX_HISTORY: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    pub history: Vec<Message>,
    pub summary: String,
}

/// In-memory session: history and optional summary. Cap history at MAX_HISTORY.
#[derive(Debug, Clone)]
pub struct Session {
    history: Vec<Message>,
    summary: String,
    path: std::path::PathBuf,
}

#[derive(Debug)]
pub enum SessionError {
    Io(String),
    Parse(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(s) => write!(f, "session io: {}", s),
            SessionError::Parse(s) => write!(f, "session parse: {}", s),
        }
    }
}

impl std::error::Error for SessionError {}

impl Session {
    /// Load session from disk; missing file or empty => empty session.
    pub async fn load(workspace_path: &Path, chat_id: &str) -> Result<Self, SessionError> {
        let path = workspace::session_file(workspace_path, chat_id);
        let (history, summary) = match fs::read_to_string(&path).await {
            Ok(s) => {
                let file: SessionFile =
                    serde_json::from_str(&s).map_err(|e| SessionError::Parse(e.to_string()))?;
                (file.history, file.summary)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), String::new()),
            Err(e) => return Err(SessionError::Io(e.to_string())),
        };
        Ok(Self {
            history,
            summary,
            path,
        })
    }

    /// Save session to disk (atomic: write to .tmp then rename). Creates parent dir if needed.
    pub async fn save(&self) -> Result<(), SessionError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| SessionError::Io(e.to_string()))?;
        }
        let file = SessionFile {
            history: self.history.clone(),
            summary: self.summary.clone(),
        };
        let json =
            serde_json::to_string_pretty(&file).map_err(|e| SessionError::Parse(e.to_string()))?;
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &json)
            .await
            .map_err(|e| SessionError::Io(e.to_string()))?;
        fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| SessionError::Io(e.to_string()))
    }

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

    /// Truncate history to last `keep` messages. Does nothing if history.len() <= keep.
    pub fn truncate_history(&mut self, keep: usize) {
        if self.history.len() > keep {
            let start = self.history.len() - keep;
            self.history.drain(..start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Message;

    fn temp_session_dir(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("icrab_session_test_{}", suffix))
    }

    #[tokio::test]
    async fn session_load_missing_returns_empty() {
        let w = temp_session_dir("missing");
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(&w).unwrap();
        let s = Session::load(&w, "nonexistent").await.unwrap();
        assert!(s.history().is_empty());
        assert!(s.summary().is_empty());
        let _ = std::fs::remove_dir_all(&w);
    }

    #[tokio::test]
    async fn session_save_load_roundtrip() {
        let w = temp_session_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(&w).unwrap();
        let path = workspace::session_file(&w, "chat1");
        let session = Session {
            history: vec![Message {
                role: Role::User,
                content: "Hi".to_string(),
                tool_call_id: None,
                tool_calls: None,
            }],
            summary: "brief".to_string(),
            path: path.clone(),
        };
        session.save().await.unwrap();
        assert!(path.is_file());
        let loaded = Session::load(&w, "chat1").await.unwrap();
        assert_eq!(loaded.history().len(), 1);
        assert_eq!(loaded.history()[0].content, "Hi");
        assert_eq!(loaded.summary(), "brief");
        let _ = std::fs::remove_dir_all(&w);
    }

    #[tokio::test]
    async fn session_save_atomic_uses_tmp_then_rename() {
        let w = temp_session_dir("atomic");
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(&w).unwrap();
        let path = workspace::session_file(&w, "chat2");
        let session = Session {
            history: vec![],
            summary: String::new(),
            path: path.clone(),
        };
        session.save().await.unwrap();
        assert!(path.is_file());
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(&w);
    }

    #[tokio::test]
    async fn session_add_messages_caps_history() {
        let w = temp_session_dir("cap");
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(&w).unwrap();
        let path = workspace::session_file(&w, "cap");
        let mut session = Session {
            history: Vec::new(),
            summary: String::new(),
            path,
        };
        for i in 0..55 {
            session.add_user_message(&format!("msg {}", i));
        }
        assert_eq!(session.history().len(), MAX_HISTORY);
        assert!(session.history().first().map(|m| m.content.as_str()) == Some("msg 5"));
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn session_truncate_history() {
        let w = temp_session_dir("truncate");
        let path = workspace::session_file(&w, "truncate");
        let mut session = Session {
            history: Vec::new(),
            summary: String::new(),
            path,
        };
        
        // Add 10 messages
        for i in 0..10 {
            session.add_user_message(&format!("msg {}", i));
        }
        assert_eq!(session.history().len(), 10);
        
        // Truncate to last 4
        session.truncate_history(4);
        assert_eq!(session.history().len(), 4);
        assert_eq!(session.history()[0].content, "msg 6");
        assert_eq!(session.history()[3].content, "msg 9");
        
        // Truncate when already shorter - should do nothing
        session.truncate_history(10);
        assert_eq!(session.history().len(), 4);
    }
}
