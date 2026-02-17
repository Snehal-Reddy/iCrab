use std::sync::Arc;
use std::time::Duration;
use serde_json::json;
use tokio::time::sleep;

use icrab::agent::subagent_manager::{SubagentManager, SubagentStatus};
use icrab::llm::HttpProvider;
use icrab::tools::registry::ToolRegistry;

mod common;
use common::{TestWorkspace, MockLlm, create_test_config};

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
        .respond_with(ResponseTemplate::new(200)
            .set_body_json(json!({
                "choices": [{
                    "message": { "content": "Done", "role": "assistant" },
                    "finish_reason": "stop"
                }]
            }))
            .set_delay(Duration::from_millis(500))
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

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    use std::time::Instant;

    // Response takes 500ms
    let delay_ms = 500;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200)
            .set_body_json(json!({
                "choices": [{
                    "message": { "content": "Done", "role": "assistant" },
                    "finish_reason": "stop"
                }]
            }))
            .set_delay(Duration::from_millis(delay_ms))
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
    for _ in 0..20 { // Max 20 * 100ms = 2s
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
    assert!(duration.as_millis() < (delay_ms as u128 * 2), "Tasks should run in parallel");
}
