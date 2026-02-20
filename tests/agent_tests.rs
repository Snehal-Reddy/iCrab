use std::sync::Arc;

use serde_json::json;
use wiremock::{Mock, ResponseTemplate};

use icrab::agent::process_message;
use icrab::agent::session::Session;
use icrab::llm::HttpProvider;
use icrab::memory::db::BrainDb;
use icrab::tools::context::ToolCtx;
use icrab::tools::file::{ReadFile, WriteFile};
use icrab::tools::registry::ToolRegistry;

mod common;
use common::{MockLlm, TestWorkspace, create_test_config};

#[tokio::test]
async fn test_agent_basic_flow() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");
    let db = Arc::new(BrainDb::open(&ws.root).unwrap());

    // Registry with just file tools
    let registry = ToolRegistry::new();
    registry.register(ReadFile);
    registry.register(WriteFile);

    // Mock LLM response
    let response_body = json!({
        "choices": [{
            "message": {
                "content": "Hello there!",
                "role": "assistant"
            },
            "finish_reason": "stop"
        }]
    });
    mock_llm.mock_chat_completion(response_body).await;

    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(123),
        channel: Some("telegram".into()),
        outbound_tx: None,
    };

    let result = process_message(
        &provider,
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_basic",
        "Hi",
        &ctx,
        &db,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "Hello there!");

    // Verify session was saved to SQLite
    let loaded = Session::load(Arc::clone(&db), "chat_basic").await.unwrap();
    assert!(
        loaded.history().iter().any(|m| m.content.contains("Hello there!")),
        "Session should contain the assistant reply"
    );
}

#[tokio::test]
async fn test_agent_tool_use_loop() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");
    let db = Arc::new(BrainDb::open(&ws.root).unwrap());

    let registry = ToolRegistry::new();
    registry.register(WriteFile);

    // Sequence of responses from LLM
    // 1. Tool call: write_file
    // 2. Final response: "Done"

    // We need to match based on the messages sent to the LLM to differentiate calls,
    // but wiremock matches are stateless/independent by default unless we use scenarios
    // or sequences.
    // Since `process_message` makes sequential calls, we can use a sequence of responses?
    // Wiremock doesn't support stateful sequences easily out of the box without extensions,
    // but we can mock based on the 'messages' content in the body.

    // 1st request: contains "Write file" (User message)
    // Response: tool call
    let tool_call_body = json!({
        "choices": [{
            "message": {
                "content": null,
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": "{\"path\": \"test.txt\", \"content\": \"success\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });

    // 2nd request: contains "tool" role message with "written" result
    // Response: "Done"
    let final_body = json!({
        "choices": [{
            "message": {
                "content": "I have written the file.",
                "role": "assistant"
            },
            "finish_reason": "stop"
        }]
    });

    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    // Mock for 1st call (User asks to write file)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Write file"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_body))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    // Mock for 2nd call (Agent reports result)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("written"))
        .respond_with(ResponseTemplate::new(200).set_body_json(final_body))
        .mount(&mock_llm.server)
        .await;

    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(123),
        channel: Some("telegram".into()),
        outbound_tx: None,
    };

    let result = process_message(
        &provider,
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_tool",
        "Write file test.txt with success",
        &ctx,
        &db,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "I have written the file.");

    // Verify file was written
    let file_path = ws.root.join("test.txt");
    assert!(file_path.exists());
    let content = std::fs::read_to_string(file_path).unwrap();
    assert_eq!(content, "success");
}

// --- §3.2 Restart mid-conversation: session load from SQLite, prior turns in context ---

#[tokio::test]
async fn test_agent_session_load_on_restart() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");
    let db = Arc::new(BrainDb::open(&ws.root).unwrap());

    let registry = ToolRegistry::new();
    registry.register(ReadFile);
    registry.register(WriteFile);

    // First "process": user says "First", assistant replies "Got First"
    let first_reply = json!({
        "choices": [{
            "message": { "content": "Got First", "role": "assistant" },
            "finish_reason": "stop"
        }]
    });
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("First"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first_reply))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(1),
        channel: Some("telegram".into()),
        outbound_tx: None,
    };

    let r1 = process_message(
        &provider,
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_restart",
        "First",
        &ctx,
        &db,
    )
    .await;
    assert!(r1.is_ok());
    assert_eq!(r1.unwrap(), "Got First");

    // Verify session stored in SQLite
    let s = Session::load(Arc::clone(&db), "chat_restart").await.unwrap();
    assert!(
        !s.history().is_empty(),
        "Session history must be non-empty after first message"
    );

    // Second "process" (restart): user says "Second". LLM must see "First" in context.
    let second_reply = json!({
        "choices": [{
            "message": { "content": "I remember: First. Now Second.", "role": "assistant" },
            "finish_reason": "stop"
        }]
    });
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("First"))
        .and(wiremock::matchers::body_string_contains("Second"))
        .respond_with(ResponseTemplate::new(200).set_body_json(second_reply))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    let r2 = process_message(
        &provider,
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_restart",
        "Second",
        &ctx,
        &db,
    )
    .await;
    assert!(r2.is_ok());
    let out = r2.unwrap();
    assert!(
        out.contains("First") && out.contains("Second"),
        "Reply should reflect prior context: {}",
        out
    );
}

// --- §3.2 LLM returns unknown tool or invalid args: no crash, error in conversation ---

#[tokio::test]
async fn test_agent_unknown_tool_completes_with_error_in_conversation() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");
    let db = Arc::new(BrainDb::open(&ws.root).unwrap());

    let registry = ToolRegistry::new();
    registry.register(ReadFile);

    // First call: LLM returns tool_calls for unknown tool
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("Use nonexistent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_x",
                        "type": "function",
                        "function": {
                            "name": "nonexistent_tool",
                            "arguments": "{}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    // Second call: after tool error "tool 'nonexistent_tool' not found", LLM returns final text
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("not found"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": { "content": "That tool is not available.", "role": "assistant" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&mock_llm.server)
        .await;

    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(1),
        channel: Some("telegram".into()),
        outbound_tx: None,
    };

    let result = process_message(
        &provider,
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_unknown_tool",
        "Use nonexistent tool",
        &ctx,
        &db,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "That tool is not available.");
}

#[tokio::test]
async fn test_agent_invalid_tool_args_completes_with_error_in_conversation() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");
    let db = Arc::new(BrainDb::open(&ws.root).unwrap());

    let registry = ToolRegistry::new();
    registry.register(ReadFile);

    // First call: LLM returns tool_calls with invalid JSON in arguments
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("Read file"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_y",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "not valid json"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    // Second call: after "Invalid JSON arguments" tool message, LLM returns final text
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("Invalid JSON"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": { "content": "I'll fix the format.", "role": "assistant" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&mock_llm.server)
        .await;

    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(1),
        channel: Some("telegram".into()),
        outbound_tx: None,
    };

    let result = process_message(
        &provider,
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_bad_args",
        "Read file foo.txt",
        &ctx,
        &db,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "I'll fix the format.");
}
