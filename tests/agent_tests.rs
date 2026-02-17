use serde_json::json;

use icrab::agent::process_message;
use icrab::llm::{HttpProvider, ToolCall, ToolCallFunction};
use icrab::tools::context::ToolCtx;
use icrab::tools::file::{ReadFile, WriteFile};
use icrab::tools::registry::ToolRegistry;

mod common;
use common::{TestWorkspace, MockLlm, create_test_config};

#[tokio::test]
async fn test_agent_basic_flow() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");

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
        "chat_basic",
        "Hi",
        &ctx
    ).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "Hello there!");

    // Verify session was saved
    let session_path = ws.root.join("sessions/chat_basic.json");
    assert!(session_path.exists());
    let content = std::fs::read_to_string(session_path).unwrap();
    assert!(content.contains("Hello there!"));
}

#[tokio::test]
async fn test_agent_tool_use_loop() {
    let ws = TestWorkspace::new();
    let mock_llm = MockLlm::new().await;
    let config = create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");

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
        "chat_tool",
        "Write file test.txt with success",
        &ctx
    ).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "I have written the file.");

    // Verify file was written
    let file_path = ws.root.join("test.txt");
    assert!(file_path.exists());
    let content = std::fs::read_to_string(file_path).unwrap();
    assert_eq!(content, "success");
}
