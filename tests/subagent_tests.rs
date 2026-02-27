use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use icrab::agent::subagent_manager::{SubagentManager, SubagentStatus};
use icrab::llm::HttpProvider;
use icrab::tools::file::ReadFile;
use icrab::tools::message::MessageTool;
use icrab::tools::registry::ToolRegistry;

mod common;
use common::{MockLlm, TestWorkspace, create_test_config};

#[tokio::test]
async fn test_subagent_spawn_and_completion() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;

    // Create config pointing to mock LLM
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));
    let registry = Arc::new(ToolRegistry::new());

    // Create SubagentManager
    let manager = Arc::new(SubagentManager::new(
        provider,
        registry,
        "gpt-4-test".to_string(),
        ws.root.clone(),
        true, // restrict to workspace
        5,    // max iterations
    ));

    // Mock LLM response
    let response_body = json!({
        "choices": [{
            "message": {
                "content": "Task completed successfully.",
                "role": "assistant"
            },
            "finish_reason": "stop"
        }]
    });
    mock_llm.mock_chat_completion(response_body).await;

    // Spawn subagent
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    let task_id = manager.spawn(
        "Analyze this text".to_string(),
        Some("analysis".to_string()),
        12345,
        Arc::new(tx),
        "telegram".to_string(),
    );

    // Poll for completion
    let mut status = SubagentStatus::Running;
    for _ in 0..20 {
        sleep(Duration::from_millis(50)).await;
        if let Some(task) = manager.get_task(&task_id) {
            status = task.status;
            if status != SubagentStatus::Running {
                break;
            }
        }
    }

    assert_eq!(status, SubagentStatus::Completed);

    let task = manager.get_task(&task_id).expect("task found");
    assert_eq!(task.result.as_deref(), Some("Task completed successfully."));
}

#[tokio::test]
async fn test_subagent_cancellation() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;

    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));
    let registry = Arc::new(ToolRegistry::new());

    let manager = Arc::new(SubagentManager::new(
        provider,
        registry,
        "gpt-4-test".to_string(),
        ws.root.clone(),
        true,
        5,
    ));

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    // Delay response by 500ms so we have time to cancel
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "choices": [{
                        "message": { "content": "Done", "role": "assistant" },
                        "finish_reason": "stop"
                    }]
                }))
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&mock_llm.server)
        .await;

    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    let task_id = manager.spawn(
        "Long running task".to_string(),
        None,
        12345,
        Arc::new(tx),
        "telegram".to_string(),
    );

    // Let it start running
    sleep(Duration::from_millis(50)).await;

    // Check it is running
    let task = manager.get_task(&task_id).unwrap();
    assert_eq!(task.status, SubagentStatus::Running);

    // Cancel
    let cancelled = manager.cancel(&task_id);
    assert!(cancelled);

    // Check status
    let task = manager.get_task(&task_id).unwrap();
    assert_eq!(task.status, SubagentStatus::Cancelled);
    assert_eq!(task.result.as_deref(), Some("Cancelled"));

    // Wait a bit to ensure it doesn't overwrite status on completion (if abort works)
    sleep(Duration::from_millis(600)).await;
    let task = manager.get_task(&task_id).unwrap();
    assert_eq!(task.status, SubagentStatus::Cancelled);
}

#[tokio::test]
async fn test_subagent_parallel_execution() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;

    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));
    let registry = Arc::new(ToolRegistry::new());

    let manager = Arc::new(SubagentManager::new(
        provider,
        registry,
        "gpt-4-test".to_string(),
        ws.root.clone(),
        true,
        5,
    ));

    use std::time::Instant;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    // Response takes 500ms
    let delay_ms = 500;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "choices": [{
                        "message": { "content": "Done", "role": "assistant" },
                        "finish_reason": "stop"
                    }]
                }))
                .set_delay(Duration::from_millis(delay_ms)),
        )
        .mount(&mock_llm.server)
        .await;

    let (tx, _rx) = tokio::sync::mpsc::channel(10);

    let start_time = Instant::now();

    // Spawn 3 subagents
    let mut task_ids = Vec::new();
    for i in 0..3 {
        let task_id = manager.spawn(
            format!("Task {}", i),
            None,
            12345,
            Arc::new(tx.clone()),
            "telegram".to_string(),
        );
        task_ids.push(task_id);
    }

    // Wait for all to complete
    let mut completed_count = 0;
    for _ in 0..20 {
        // Max 20 * 100ms = 2s
        sleep(Duration::from_millis(100)).await;

        completed_count = 0;
        for id in &task_ids {
            if let Some(task) = manager.get_task(id) {
                if task.status == SubagentStatus::Completed {
                    completed_count += 1;
                }
            }
        }
        if completed_count == 3 {
            break;
        }
    }

    let duration = start_time.elapsed();

    assert_eq!(completed_count, 3, "All tasks should complete");

    // If sequential: 3 * 500ms = 1500ms + overhead
    // If parallel: max(500ms) = 500ms + overhead
    // We assert it took less than 1.2s to be safe
    println!("Execution took: {:?}", duration);
    assert!(
        duration.as_millis() < (delay_ms as u128 * 2),
        "Tasks should run in parallel"
    );
}

// --- §3.4 Subagent result reaches user: subagent calls message → outbound with same chat_id ---

#[tokio::test]
async fn test_subagent_message_tool_sends_to_outbound() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));

    let registry = Arc::new(ToolRegistry::new());
    registry.register(MessageTool);

    let manager = Arc::new(SubagentManager::new(
        provider,
        registry,
        "gpt-4-test".to_string(),
        ws.root.clone(),
        true,
        5,
    ));

    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    // 1st subagent LLM call: return message tool call
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Analyze"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "message",
                            "arguments": "{\"text\": \"Subagent result for user\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    // 2nd call: final content
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("sent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": { "content": "Done.", "role": "assistant" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&mock_llm.server)
        .await;

    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel(10);
    let chat_id = 999;
    let task_id = manager.spawn(
        "Analyze and report".to_string(),
        None,
        chat_id,
        Arc::new(outbound_tx),
        "telegram".to_string(),
    );

    // Receive the message tool output
    let out = tokio::time::timeout(Duration::from_secs(3), outbound_rx.recv())
        .await
        .expect("timeout waiting for outbound message")
        .expect("channel open");
    assert_eq!(out.chat_id, chat_id);
    assert_eq!(out.text, "Subagent result for user");

    // Wait for task to complete
    for _ in 0..30 {
        sleep(Duration::from_millis(50)).await;
        if let Some(task) = manager.get_task(&task_id) {
            if task.status != SubagentStatus::Running {
                break;
            }
        }
    }
    let task = manager.get_task(&task_id).expect("task found");
    assert_eq!(task.status, SubagentStatus::Completed);
}

// --- §3.4 Subagent hits max iterations: clear result, no hang ---

#[tokio::test]
async fn test_subagent_max_iterations_returns_clean_result() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));

    let registry = Arc::new(ToolRegistry::new());
    registry.register(ReadFile);

    let manager = Arc::new(SubagentManager::new(
        provider,
        registry,
        "gpt-4-test".to_string(),
        ws.root.clone(),
        true,
        2, // max_iterations = 2
    ));

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    // Every LLM call returns tool_calls so we never get finish_reason: stop
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\": \"memory/MEMORY.md\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .mount(&mock_llm.server)
        .await;

    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    let task_id = manager.spawn(
        "Loop task".to_string(),
        None,
        1,
        Arc::new(tx),
        "telegram".to_string(),
    );

    for _ in 0..40 {
        sleep(Duration::from_millis(50)).await;
        if let Some(task) = manager.get_task(&task_id) {
            if task.status != SubagentStatus::Running {
                assert_eq!(task.status, SubagentStatus::Completed);
                let result = task.result.as_deref().unwrap_or("");
                assert!(
                    result.contains("Max iterations"),
                    "result should indicate max iterations: {}",
                    result
                );
                return;
            }
        }
    }
    panic!("subagent did not complete within timeout");
}

// --- §3.4 Main agent does not block on spawn (async path) ---

#[tokio::test]
async fn test_main_agent_spawn_returns_before_subagent_completes() {
    use std::time::Instant;

    use icrab::agent::process_message;
    use icrab::tools::context::ToolCtx;
    use icrab::tools::spawn::SpawnTool;

    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));

    let subagent_registry = Arc::new(ToolRegistry::new());
    let manager = Arc::new(SubagentManager::new(
        Arc::clone(&provider),
        subagent_registry,
        "gpt-4-test".to_string(),
        ws.root.clone(),
        true,
        5,
    ));

    let registry = ToolRegistry::new();
    registry.register(SpawnTool::new(Arc::clone(&manager)));

    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    // 1st main agent call: spawn tool
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Start background"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_spawn",
                        "type": "function",
                        "function": {
                            "name": "spawn",
                            "arguments": "{\"task\": \"Long task\", \"label\": \"bg\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    // 2nd main agent call: after spawn tool result "Subagent 'bg' started...", return final reply.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Subagent 'bg' started"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": { "content": "Task started.", "role": "assistant" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&mock_llm.server)
        .await;

    // Subagent LLM: slow response. Match only subagent (system prompt contains this).
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("You are a subagent"))
        .and(body_string_contains("Long task"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "choices": [{
                        "message": { "content": "Subagent done.", "role": "assistant" },
                        "finish_reason": "stop"
                    }]
                }))
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&mock_llm.server)
        .await;

    let (_out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(1),
        channel: Some("telegram".into()),
        outbound_tx: Some(Arc::new(_out_tx)),
        delivered: Default::default(),
    };

    let db = std::sync::Arc::new(icrab::memory::db::BrainDb::open(&ws.root).unwrap());
    let start = Instant::now();
    let result = process_message(
        provider.as_ref(),
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_spawn",
        "Start background task",
        &ctx,
        &db,
    )
    .await;
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "Task started.");
    assert!(
        elapsed.as_millis() < 600,
        "Main agent should return before subagent (subagent delay 800ms); took {:?}",
        elapsed
    );
}
