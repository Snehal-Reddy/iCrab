//! Timer loop: read workspace/HEARTBEAT.md, push one InboundMsg per task to agent.
//!
//! Each markdown bullet (`- `) in HEARTBEAT.md becomes its own agent run (one-shot, no session).
//! Heartbeat pushes onto the same `inbound_tx` as Telegram and cron; the main loop branches on
//! `channel == "heartbeat"` to call `process_heartbeat_message` instead of `process_message`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::telegram::InboundMsg;

/// Parse markdown bullet tasks from HEARTBEAT.md content.
///
/// Lines whose trimmed form starts with `"- "` are tasks; everything else is ignored.
/// Inner whitespace around the task text is trimmed; blank tasks are dropped.
pub fn parse_tasks(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("- ")
                .map(|task| task.trim().to_string())
        })
        .filter(|task| !task.is_empty())
        .collect()
}

/// Read and parse tasks from `workspace/HEARTBEAT.md`.
///
/// Returns an empty vec if the file does not exist or cannot be read.
/// Sync I/O is fine: this is called at most once per N-minute tick.
fn read_tasks(workspace: &Path) -> Vec<String> {
    let path = workspace.join("HEARTBEAT.md");
    if !path.exists() {
        return vec![];
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    parse_tasks(&content)
}

/// Spawn the heartbeat runner.
///
/// Every `interval_minutes` minutes: read `HEARTBEAT.md`, and for each task push one
/// `InboundMsg { channel: "heartbeat" }` onto `inbound_tx`.  The main loop will call
/// `process_heartbeat_message` once per message — N agent calls per tick (N = tasks).
///
/// `last_chat_id` is loaded on each tick to find the current active Telegram chat.
/// If it is `0` (no user has messaged yet) the messages are still pushed; main.rs
/// drops the reply in that case.
///
/// # Panics
/// Panics if `interval_minutes == 0` (caller must check before calling).
pub fn spawn_heartbeat_runner(
    workspace: PathBuf,
    interval_minutes: u64,
    inbound_tx: mpsc::Sender<InboundMsg>,
    last_chat_id: Arc<AtomicI64>,
) -> tokio::task::JoinHandle<()> {
    assert!(
        interval_minutes >= 1,
        "heartbeat interval_minutes must be >= 1"
    );
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_minutes * 60));
        // Skip the immediately-firing first tick so the first real tick is one full interval out.
        interval.tick().await;
        loop {
            interval.tick().await;
            let tasks = read_tasks(&workspace);
            if tasks.is_empty() {
                continue;
            }
            let chat_id = last_chat_id.load(Ordering::Relaxed);
            for task in tasks {
                let msg = InboundMsg {
                    chat_id,
                    user_id: 0,
                    text: format!("[Heartbeat Task] {task}"),
                    channel: "heartbeat".to_string(),
                };
                if inbound_tx.send(msg).await.is_err() {
                    // Receiver closed (main loop exited); nothing more to do.
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_tasks ---

    #[test]
    fn parse_empty() {
        assert!(parse_tasks("").is_empty());
    }

    #[test]
    fn parse_single_bullet() {
        assert_eq!(parse_tasks("- Check weather"), ["Check weather"]);
    }

    #[test]
    fn parse_multiple_bullets() {
        let tasks = parse_tasks("- First\n- Second\n- Third");
        assert_eq!(tasks, ["First", "Second", "Third"]);
    }

    #[test]
    fn parse_skips_non_bullet_lines() {
        let content = "# Heartbeat\n\nProse.\n\n- Do thing\n<!-- comment -->\n- Another";
        assert_eq!(parse_tasks(content), ["Do thing", "Another"]);
    }

    #[test]
    fn parse_unicode_task() {
        assert_eq!(parse_tasks("- こんにちは 🦀"), ["こんにちは 🦀"]);
    }

    #[test]
    fn parse_strips_inner_whitespace() {
        assert_eq!(parse_tasks("- \t  trim me  \t"), ["trim me"]);
    }

    #[test]
    fn parse_skips_empty_bullets() {
        // A bare "- " with nothing after it is dropped.
        assert_eq!(parse_tasks("- \n- real task"), ["real task"]);
    }

    #[test]
    fn parse_mixed_indentation_ignored() {
        // Lines that do NOT start with "- " after trimming are skipped.
        let tasks = parse_tasks("  - indented\n- normal");
        assert_eq!(tasks, ["indented", "normal"]);
    }

    // --- read_tasks ---

    #[test]
    fn read_tasks_returns_empty_when_file_missing() {
        let dir = std::env::temp_dir().join("icrab_hb_no_file_test");
        // Ensure no HEARTBEAT.md exists in this temp dir.
        let _ = std::fs::remove_file(dir.join("HEARTBEAT.md"));
        assert!(read_tasks(&dir).is_empty());
    }

    #[test]
    fn read_tasks_parses_file() {
        let dir = std::env::temp_dir().join("icrab_hb_read_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("HEARTBEAT.md"), "- Alpha\n- Beta\n").unwrap();
        let tasks = read_tasks(&dir);
        assert_eq!(tasks, ["Alpha", "Beta"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- message format ---

    #[tokio::test]
    async fn messages_have_correct_format_and_channel() {
        use std::sync::atomic::AtomicI64;
        use tokio::sync::mpsc;

        let dir = std::env::temp_dir().join("icrab_hb_msg_fmt_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("HEARTBEAT.md"), "- Task A\n- Task B\n").unwrap();

        let last_chat_id = Arc::new(AtomicI64::new(42));
        let tasks = read_tasks(&dir);
        assert_eq!(tasks.len(), 2);

        let (tx, mut rx) = mpsc::channel(8);
        let chat_id = last_chat_id.load(Ordering::Relaxed);
        for task in &tasks {
            tx.send(InboundMsg {
                chat_id,
                user_id: 0,
                text: format!("[Heartbeat Task] {task}"),
                channel: "heartbeat".to_string(),
            })
            .await
            .unwrap();
        }
        drop(tx);

        let a = rx.recv().await.unwrap();
        assert_eq!(a.chat_id, 42);
        assert_eq!(a.text, "[Heartbeat Task] Task A");
        assert_eq!(a.channel, "heartbeat");
        assert_eq!(a.user_id, 0);

        let b = rx.recv().await.unwrap();
        assert_eq!(b.text, "[Heartbeat Task] Task B");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
