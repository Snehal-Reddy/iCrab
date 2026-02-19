//! Build system prompt: identity, bootstrap files, memory snippet, skills summary, tool list.

use std::path::Path;

use time::OffsetDateTime;

use crate::llm::{Message, Role};
use crate::workspace;

/// Build full message list for the LLM: [system, …history…, user].
/// System prompt order: identity → bootstrap (AGENT.md, USER.md, IDENTITY.md) → memory snippet →
/// skills → tool list → current session (chat_id). Then history and current user message.
#[allow(clippy::too_many_arguments)]
pub fn build_messages(
    workspace_path: &Path,
    history: &[Message],
    summary: &str,
    user_message: &str,
    chat_id: Option<&str>,
    skills_summary: &str,
    tool_summaries: &[String],
    today_yyyymmdd: Option<&str>,
) -> Vec<Message> {
    let mut system = String::new();

    // Identity: current date/time (human-readable + timezone) and Unix, workspace
    let now = OffsetDateTime::now_utc();
    let now_unix = now.unix_timestamp();
    system.push_str("You are iCrab, a minimal personal AI assistant. ");
    system.push_str("Current time: ");
    system.push_str(&format!("{} {} {} UTC. ", now.weekday(), now.date(), now.time()));
    system.push_str("Unix: ");
    system.push_str(&now_unix.to_string());
    system.push_str(". Workspace: ");
    system.push_str(workspace_path.to_string_lossy().as_ref());
    system.push_str(".\n\n");

    // Bootstrap files (if present)
    for (name, path) in [
        ("AGENT", workspace::agent_md(workspace_path)),
        ("USER", workspace::user_md(workspace_path)),
        ("IDENTITY", workspace::identity_md(workspace_path)),
    ] {
        if let Ok(s) = std::fs::read_to_string(&path) {
            let t = s.trim();
            if !t.is_empty() {
                system.push_str("--- ");
                system.push_str(name);
                system.push_str(" ---\n");
                system.push_str(t);
                system.push_str("\n\n");
            }
        }
    }

    // Memory snippet (MEMORY.md + recent daily notes, last 3 days when today given)
    let mem = workspace::read_memory_snippet(
        workspace_path,
        today_yyyymmdd,
        workspace::RECENT_DAILY_DAYS,
    );
    if !mem.is_empty() {
        system.push_str("--- Memory ---\n");
        system.push_str(&mem);
        system.push_str("\n\n");
    }

    // Skills
    if !skills_summary.is_empty() {
        system.push_str("--- Skills ---\n");
        system.push_str(skills_summary);
        system.push_str("\n\n");
    }

    // Tools
    system.push_str("--- Tools ---\n");
    if tool_summaries.is_empty() {
        system.push_str("No tools registered.\n");
    } else {
        for line in tool_summaries {
            system.push_str(line);
            system.push('\n');
        }
    }

    // Current session
    if let Some(cid) = chat_id {
        system.push_str("\nCurrent chat: ");
        system.push_str(cid);
        system.push_str(".\n");
    }
    if !summary.is_empty() {
        system.push_str("\nSession summary: ");
        system.push_str(summary);
        system.push('\n');
    }

    let system_msg = Message {
        role: Role::System,
        content: system.trim().to_string(),
        tool_call_id: None,
        tool_calls: None,
    };

    let mut messages = Vec::with_capacity(2 + history.len());
    messages.push(system_msg);
    messages.extend(history.iter().cloned());
    messages.push(Message {
        role: Role::User,
        content: user_message.to_string(),
        tool_call_id: None,
        tool_calls: None,
    });
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEKDAYS: &[&str] = &[
        "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday",
    ];

    #[test]
    fn system_prompt_includes_human_readable_time_and_unix() {
        let workspace = std::env::temp_dir();
        let messages = build_messages(
            &workspace,
            &[],
            "",
            "hello",
            None,
            "",
            &[],
            None,
        );
        let system = &messages[0].content;
        assert!(
            system.contains("Current time:"),
            "system prompt should include 'Current time:'"
        );
        assert!(
            system.contains(" UTC."),
            "system prompt should include ' UTC.'"
        );
        assert!(
            system.contains("Unix: "),
            "system prompt should include 'Unix: '"
        );
        let has_weekday = WEEKDAYS.iter().any(|w| system.contains(w));
        assert!(
            has_weekday,
            "system prompt should include a weekday (e.g. Wednesday)"
        );
        // Unix timestamp should be a positive number after "Unix: "
        let unix_prefix = "Unix: ";
        let start = system.find(unix_prefix).unwrap() + unix_prefix.len();
        let rest = &system[start..];
        let end = rest.find('.').unwrap_or(rest.len());
        let unix_str = rest[..end].trim();
        assert!(
            unix_str.parse::<u64>().is_ok(),
            "Unix value should be numeric, got: {}",
            unix_str
        );
    }
}
