use std::sync::Arc;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use icrab::agent::subagent_manager::SubagentManager;
use icrab::llm::HttpProvider;
use icrab::tools::registry::ToolRegistry;
use icrab::tools::subagent::SubagentTool;
use icrab::tools::{Tool, ToolCtx};

mod common;
use common::{TestWorkspace, MockLlm, create_test_config};

#[tokio::test]
async fn subagent_tool_returns_result_synchronously() {
    // Setup
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));
    // Empty registry for subagent (it only needs llm to answer)
    let subagent_registry = Arc::new(ToolRegistry::new());

    let manager = Arc::new(SubagentManager::new(
        provider.clone(),
        subagent_registry,
        "gpt-4-test".to_string(),
        ws.root.clone(),
        true,
        5,
    ));
    
    let tool = SubagentTool::new(manager);

    // Mock response for the subagent's internal LLM call
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": "Subagent completed the task.",
                    "role": "assistant"
                },
                "finish_reason": "stop"
            }]
        })))
        .expect(1) // Expect exactly one call
        .mount(&mock_llm.server)
        .await;

    // Execute tool
    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(123),
        channel: Some("telegram".to_string()),
        outbound_tx: None, 
    };

    let args = json!({
        "task": "Say hello",
        "label": "greet"
    });

    let result = tool.execute(&ctx, &args).await;

    // Assertions
    assert!(!result.is_error, "Result should not be error: {}", result.for_llm);
    assert!(!result.async_, "Result should be synchronous");
    assert!(result.for_llm.contains("Subagent 'greet' completed"));
    assert!(result.for_llm.contains("Subagent completed the task."));
    assert_eq!(result.for_user.as_deref(), Some("Subagent completed the task."));
}

#[tokio::test]
async fn subagent_tool_missing_task_returns_error() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = Arc::new(HttpProvider::from_config(&config).expect("provider"));
    let registry = Arc::new(ToolRegistry::new());
    let manager = Arc::new(SubagentManager::new(
        provider, registry, "m".into(), ws.root.clone(), true, 5
    ));
    let tool = SubagentTool::new(manager);

    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(123),
        channel: None,
        outbound_tx: None,
    };

    let result = tool.execute(&ctx, &json!({})).await;
    assert!(result.is_error);
    assert!(result.for_llm.contains("task"));
}
