//! Session history summarization: compress old messages into concise summaries.

use crate::agent::session::Session;
use crate::llm::{HttpProvider, LlmError, Message, Role};

// --- Constants ---

const KEEP_RECENT_MESSAGES: usize = 4;
pub const SUMMARIZE_THRESHOLD: usize = 20;
const MAX_MESSAGE_TOKENS_RATIO: f64 = 0.5; // 50% of context window
const DEFAULT_CONTEXT_WINDOW: usize = 128_000; // tokens
const SUMMARY_MAX_TOKENS: usize = 1024;
const SUMMARY_TEMPERATURE: f64 = 0.2;
const MULTI_PASS_THRESHOLD: usize = 10; // messages

// --- Error Type ---

#[derive(Debug)]
pub enum SummarizeError {
    Llm(LlmError),
    EmptyBatch,
}

impl std::fmt::Display for SummarizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummarizeError::Llm(e) => write!(f, "summarize llm: {}", e),
            SummarizeError::EmptyBatch => write!(f, "summarize: empty batch"),
        }
    }
}

impl std::error::Error for SummarizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SummarizeError::Llm(e) => Some(e),
            _ => None,
        }
    }
}

impl From<LlmError> for SummarizeError {
    fn from(e: LlmError) -> Self {
        SummarizeError::Llm(e)
    }
}

// --- Public API ---

/// Summarize session history if it exceeds threshold.
/// Returns true if summarization occurred, false otherwise.
pub async fn summarize_if_needed(
    llm: &HttpProvider,
    session: &mut Session,
    model: &str,
) -> Result<bool, SummarizeError> {
    if !should_summarize(session.history()) {
        return Ok(false);
    }

    let to_summarize = &session.history()[..session.history().len() - KEEP_RECENT_MESSAGES];
    let max_tokens = (DEFAULT_CONTEXT_WINDOW as f64 * MAX_MESSAGE_TOKENS_RATIO) as usize;
    let (valid_messages, omitted) = filter_valid_messages(to_summarize, max_tokens);

    if valid_messages.is_empty() {
        // Fallback: truncate to keep recent + some buffer
        session.truncate_history(KEEP_RECENT_MESSAGES + 10);
        return Ok(false);
    }

    let existing_summary = session.summary().to_string();
    let new_summary = if valid_messages.len() > MULTI_PASS_THRESHOLD {
        // Multi-pass: split, summarize each half, then merge
        let mid = valid_messages.len() / 2;
        let part1 = &valid_messages[..mid];
        let part2 = &valid_messages[mid..];

        let s1 = summarize_batch(llm, part1, "", model).await?;
        let s2 = summarize_batch(llm, part2, "", model).await?;
        merge_summaries(llm, &s1, &s2, model).await.unwrap_or_else(|_| {
            // Fallback: concatenate
            format!("{}\n\n{}", s1, s2)
        })
    } else {
        // Single-pass
        summarize_batch(llm, &valid_messages, &existing_summary, model).await?
    };

    // Append note if messages were omitted
    let final_summary = if omitted && !new_summary.is_empty() {
        format!(
            "{}\n\n[Note: Some oversized messages were omitted from this summary for efficiency.]",
            new_summary
        )
    } else {
        new_summary
    };

    // Update session: set summary and truncate history
    if !final_summary.is_empty() {
        let updated_summary = if existing_summary.is_empty() {
            final_summary
        } else {
            format!("{}\n\n{}", existing_summary, final_summary)
        };
        session.set_summary(updated_summary);
    }
    session.truncate_history(KEEP_RECENT_MESSAGES);

    Ok(true)
}

// --- Helper Functions ---

fn should_summarize(history: &[Message]) -> bool {
    history.len() > SUMMARIZE_THRESHOLD
}

fn estimate_tokens(text: &str) -> usize {
    // Fast approximation: char_count / 3 (accounts for CJK and multi-byte)
    text.chars().count() / 3
}

fn filter_valid_messages(
    messages: &[Message],
    max_tokens: usize,
) -> (Vec<Message>, bool) {
    let mut valid = Vec::new();
    let mut omitted = false;

    for msg in messages {
        // Only include user and assistant messages (skip tool messages)
        if msg.role != Role::User && msg.role != Role::Assistant {
            continue;
        }

        let tokens = estimate_tokens(&msg.content);
        if tokens > max_tokens {
            omitted = true;
            continue;
        }

        valid.push(msg.clone());
    }

    (valid, omitted)
}

fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut buf = String::with_capacity(messages.len() * 200); // Rough estimate

    for msg in messages {
        let role_label = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            _ => continue, // Shouldn't happen after filtering, but be safe
        };
        buf.push_str(role_label);
        buf.push_str(": ");
        buf.push_str(&msg.content);
        buf.push_str("\n\n");
    }

    buf
}

async fn summarize_batch(
    llm: &HttpProvider,
    messages: &[Message],
    existing_summary: &str,
    model: &str,
) -> Result<String, SummarizeError> {
    if messages.is_empty() {
        return Err(SummarizeError::EmptyBatch);
    }

    let system_prompt = "You are a conversation compaction engine. Summarize older chat history into concise context for future turns. Preserve: user preferences, commitments, decisions, unresolved tasks, key facts. Omit: filler, repeated chit-chat, verbose tool logs. Output plain text bullet points only.";

    let formatted = format_messages_for_summary(messages);
    let user_prompt = if existing_summary.is_empty() {
        format!(
            "Summarize the following conversation history for context preservation. Keep it short (max 12 bullet points).\n\nCONVERSATION:\n{}",
            formatted
        )
    } else {
        format!(
            "Summarize the following conversation history for context preservation. Keep it short (max 12 bullet points).\n\nExisting context: {}\n\nCONVERSATION:\n{}",
            existing_summary, formatted
        )
    };

    let msgs = vec![
        Message {
            role: Role::System,
            content: system_prompt.to_string(),
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            role: Role::User,
            content: user_prompt,
            tool_call_id: None,
            tool_calls: None,
        },
    ];

    let response = llm
        .chat_with_params(&msgs, &[], model, Some(SUMMARY_TEMPERATURE), Some(SUMMARY_MAX_TOKENS))
        .await?;

    Ok(response.content.trim().to_string())
}

async fn merge_summaries(
    llm: &HttpProvider,
    s1: &str,
    s2: &str,
    model: &str,
) -> Result<String, SummarizeError> {
    let merge_prompt = format!(
        "Merge these two conversation summaries into one cohesive summary:\n\n1: {}\n\n2: {}",
        s1, s2
    );

    let msgs = vec![Message {
        role: Role::User,
        content: merge_prompt,
        tool_call_id: None,
        tool_calls: None,
    }];

    let response = llm
        .chat_with_params(&msgs, &[], model, Some(SUMMARY_TEMPERATURE), Some(SUMMARY_MAX_TOKENS))
        .await?;

    Ok(response.content.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_summarize_returns_false_when_below_threshold() {
        let history = vec![Message {
            role: Role::User,
            content: "test".to_string(),
            tool_call_id: None,
            tool_calls: None,
        }; SUMMARIZE_THRESHOLD];
        assert!(!should_summarize(&history));
    }

    #[test]
    fn should_summarize_returns_true_when_above_threshold() {
        let history = vec![Message {
            role: Role::User,
            content: "test".to_string(),
            tool_call_id: None,
            tool_calls: None,
        }; SUMMARIZE_THRESHOLD + 1];
        assert!(should_summarize(&history));
    }

    #[test]
    fn estimate_tokens_basic() {
        // "hello" = 5 chars / 3 ≈ 1 token
        assert_eq!(estimate_tokens("hello"), 1);
        // "hello world" = 11 chars / 3 ≈ 3 tokens
        assert_eq!(estimate_tokens("hello world"), 3);
    }

    #[test]
    fn filter_valid_messages_skips_tool_messages() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "test".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::Tool,
                content: "tool result".to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
            },
            Message {
                role: Role::Assistant,
                content: "response".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (valid, omitted) = filter_valid_messages(&messages, 1000);
        assert_eq!(valid.len(), 2); // User and Assistant only
        assert!(!omitted);
    }

    #[test]
    fn filter_valid_messages_skips_oversized() {
        // Create a message that will estimate to more than max_tokens
        // max_tokens = 128000 * 0.5 = 64000
        // To exceed this, we need chars / 3 > 64000
        // With integer division, chars / 3 = 64000 when chars = 192000
        // So we need chars >= 192003 to get 64001 tokens
        let max_tokens = (DEFAULT_CONTEXT_WINDOW as f64 * MAX_MESSAGE_TOKENS_RATIO) as usize;
        let min_chars_to_exceed = max_tokens * 3 + 3; // Ensure it exceeds (64001 tokens)
        let large_content = "x".repeat(min_chars_to_exceed);
        let messages = vec![
            Message {
                role: Role::User,
                content: "normal".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::User,
                content: large_content,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (valid, omitted) = filter_valid_messages(&messages, max_tokens);
        assert_eq!(valid.len(), 1);
        assert!(omitted);
    }

    #[test]
    fn format_messages_for_summary_formats_correctly() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "Hello".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::Assistant,
                content: "Hi there".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let formatted = format_messages_for_summary(&messages);
        assert!(formatted.contains("User: Hello"));
        assert!(formatted.contains("Assistant: Hi there"));
    }
}
