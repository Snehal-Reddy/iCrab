//! Agent loop: context builder, session load/save/summarize, LLM + tool_calls loop, subagent runner.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::session::{Session, SessionError};
use crate::agent::subagent_manager::{SubagentManager, SubagentStatus};
use crate::llm::{HttpProvider, Message, Role};
use crate::memory::db::BrainDb;
use crate::skills::{self, SkillsError};
use crate::telegram::OutboundMsg;
use crate::tools::context::ToolCtx;
use crate::tools::registry::ToolRegistry;
use context::build_messages;

pub mod context;
pub mod session;
pub mod subagent_manager;
pub mod summarize;

const MAX_ITERATIONS: u32 = 20;

#[derive(Debug)]
pub enum AgentError {
    Llm(crate::llm::LlmError),
    Session(String),
    Context(String),
    Tool(String),
    MaxIterations,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Llm(e) => write!(f, "agent llm: {}", e),
            AgentError::Session(s) => write!(f, "agent session: {}", s),
            AgentError::Context(s) => write!(f, "agent context: {}", s),
            AgentError::Tool(s) => write!(f, "agent tool: {}", s),
            AgentError::MaxIterations => write!(f, "agent: max iterations reached"),
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AgentError::Llm(e) => Some(e),
            _ => None,
        }
    }
}

impl From<crate::llm::LlmError> for AgentError {
    fn from(e: crate::llm::LlmError) -> Self {
        AgentError::Llm(e)
    }
}

impl From<SessionError> for AgentError {
    fn from(e: SessionError) -> Self {
        AgentError::Session(e.to_string())
    }
}

impl From<SkillsError> for AgentError {
    fn from(e: SkillsError) -> Self {
        AgentError::Context(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Inner agent loop (shared by main agent and subagent)
// ---------------------------------------------------------------------------

/// Pure agent loop: given messages and tools, call LLM repeatedly until no
/// tool_calls remain.  Returns final assistant content.  No session I/O.
pub async fn run_agent_loop(
    llm: &HttpProvider,
    registry: &ToolRegistry,
    mut messages: Vec<Message>,
    tool_ctx: &ToolCtx,
    model: &str,
    max_iterations: u32,
) -> Result<String, AgentError> {
    let tool_defs = registry.to_tool_defs();

    for _iter in 1..=max_iterations {
        let response = llm.chat(&messages, &tool_defs, model).await?;

        if response.tool_calls.is_empty() {
            let content = response.content.trim().to_string();
            return Ok(if content.is_empty() {
                "(No response)".to_string()
            } else {
                content
            });
        }

        messages.push(Message {
            role: Role::Assistant,
            content: response.content,
            tool_call_id: None,
            tool_calls: Some(response.tool_calls.clone()),
        });

        for tc in &response.tool_calls {
            let args = match serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                Ok(v) => v,
                Err(e) => {
                    messages.push(Message {
                        role: Role::Tool,
                        content: format!("Invalid JSON arguments: {}", e),
                        tool_call_id: Some(tc.id.clone()),
                        tool_calls: None,
                    });
                    continue;
                }
            };

            let result = registry.execute(tool_ctx, &tc.function.name, &args).await;

            if let Some(ref text) = result.for_user {
                if !result.silent {
                    if let (Some(tx), Some(cid)) = (tool_ctx.outbound_tx.as_ref(), tool_ctx.chat_id)
                    {
                        let _ = tx.try_send(OutboundMsg {
                            chat_id: cid,
                            text: text.clone(),
                            channel: tool_ctx
                                .channel
                                .clone()
                                .unwrap_or_else(|| "telegram".to_string()),
                        });
                    }
                }
            }

            messages.push(Message {
                role: Role::Tool,
                content: result.for_llm,
                tool_call_id: Some(tc.id.clone()),
                tool_calls: None,
            });
        }
    }

    Ok("Max iterations reached.".to_string())
}

// ---------------------------------------------------------------------------
// Main agent entry point (session-aware wrapper around run_agent_loop)
// ---------------------------------------------------------------------------

/// Process one user message: load session, build context, run LLM loop until
/// no tool_calls, persist session and return reply.
pub async fn process_message(
    llm: &HttpProvider,
    registry: &ToolRegistry,
    workspace_path: &Path,
    model: &str,
    timezone: &str,
    chat_id: &str,
    user_message: &str,
    tool_ctx: &ToolCtx,
    db: &Arc<BrainDb>,
) -> Result<String, AgentError> {
    let mut session = Session::load(Arc::clone(db), chat_id).await?;

    // Check if summarization is needed (before building context so summary is included)
    if session.history().len() > summarize::SUMMARIZE_THRESHOLD {
        if let Err(e) = summarize::summarize_if_needed(llm, &mut session, model).await {
            eprintln!("Warning: summarization failed: {}", e);
            // Continue anyway — summarization is optimization
        }
    }

    let skills_summary = skills::build_skills_summary(workspace_path)?;
    let tool_summaries = registry.summaries();

    let today = crate::workspace::today_yyyymmdd();
    let messages = build_messages(
        workspace_path,
        timezone,
        session.history(),
        session.summary(),
        user_message,
        Some(chat_id),
        &skills_summary,
        &tool_summaries,
        Some(&today),
    );
    session.add_user_message(user_message);

    let final_content =
        run_agent_loop(llm, registry, messages, tool_ctx, model, MAX_ITERATIONS).await?;

    session.add_assistant_message(&final_content, None);
    session.save().await?;
    Ok(final_content)
}

// ---------------------------------------------------------------------------
// Heartbeat agent entry point (one-shot, no session)
// ---------------------------------------------------------------------------

/// One-shot run for heartbeat: same context as `process_message` but with empty
/// history and summary.  No session load or save.
pub async fn process_heartbeat_message(
    llm: &HttpProvider,
    registry: &ToolRegistry,
    workspace_path: &Path,
    model: &str,
    timezone: &str,
    chat_id: &str,
    user_message: &str,
    tool_ctx: &ToolCtx,
) -> Result<String, AgentError> {
    let skills_summary = skills::build_skills_summary(workspace_path)?;
    let tool_summaries = registry.summaries();
    let today = crate::workspace::today_yyyymmdd();
    let messages = build_messages(
        workspace_path,
        timezone,
        &[],
        "",
        user_message,
        Some(chat_id),
        &skills_summary,
        &tool_summaries,
        Some(&today),
    );
    run_agent_loop(llm, registry, messages, tool_ctx, model, MAX_ITERATIONS).await
}

// ---------------------------------------------------------------------------
// Subagent runner (background; called by SubagentManager::spawn)
// ---------------------------------------------------------------------------

/// Run a subagent to completion.  Builds a minimal system prompt (with skills
/// and tool summaries), runs `run_agent_loop`, then updates the manager task
/// state.  Called inside `tokio::spawn` — must not panic.
pub(crate) async fn run_subagent(
    manager: Arc<SubagentManager>,
    task_id: String,
    task: String,
    _label: Option<String>,
    chat_id: i64,
    outbound_tx: Arc<mpsc::Sender<OutboundMsg>>,
    channel: String,
) {
    // --- Build system prompt ---
    let mut system = String::from(
        "You are a subagent. Complete the given task independently and report the result.\n\
         You have access to tools - use them as needed to complete your task.\n\
         After completing the task, provide a clear summary of what was done.\n\
         Send your result to the user with the message tool.\n",
    );

    // Skills
    match skills::build_skills_summary(manager.workspace()) {
        Ok(ref s) if !s.is_empty() => {
            system.push_str("\n--- Skills ---\n");
            system.push_str(s);
            system.push('\n');
        }
        Err(e) => {
            eprintln!("subagent {}: skills error: {}", task_id, e);
        }
        _ => {}
    }

    // Tool summaries
    let summaries = manager.registry().summaries();
    if !summaries.is_empty() {
        system.push_str("\n--- Tools ---\n");
        for line in &summaries {
            system.push_str(line);
            system.push('\n');
        }
    }

    let messages = vec![
        Message {
            role: Role::System,
            content: system,
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            role: Role::User,
            content: task,
            tool_call_id: None,
            tool_calls: None,
        },
    ];

    let tool_ctx = ToolCtx {
        workspace: manager.workspace().clone(),
        restrict_to_workspace: manager.restrict_to_workspace(),
        chat_id: Some(chat_id),
        channel: Some(channel),
        outbound_tx: Some(outbound_tx),
    };

    match run_agent_loop(
        manager.llm(),
        manager.registry(),
        messages,
        &tool_ctx,
        manager.model(),
        manager.max_iterations(),
    )
    .await
    {
        Ok(content) => {
            manager.complete_task(&task_id, SubagentStatus::Completed, Some(content));
        }
        Err(e) => {
            eprintln!("subagent {} error: {}", task_id, e);
            manager.complete_task(&task_id, SubagentStatus::Failed, Some(e.to_string()));
        }
    }
}
