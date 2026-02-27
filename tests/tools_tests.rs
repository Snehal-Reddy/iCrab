use serde_json::json;
use wiremock::{Mock, ResponseTemplate, matchers::method};

use icrab::tools::context::ToolCtx;
use icrab::tools::file::{AppendFile, EditFile, ListDir, ReadFile, WriteFile};
use icrab::tools::message::MessageTool;
use icrab::tools::registry::Tool;
use icrab::tools::web::{WebFetchTool, web_client};

mod common;
use common::TestWorkspace;

#[tokio::test]
async fn test_file_ops() {
    let ws = TestWorkspace::new();
    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: None,
        channel: None,
        outbound_tx: None,
        delivered: Default::default(),
    };

    // 1. Write file
    let write_tool = WriteFile;
    let res = write_tool
        .execute(
            &ctx,
            &json!({
                "path": "test.txt",
                "content": "Hello World"
            }),
        )
        .await;
    assert!(!res.is_error, "Write failed: {}", res.for_llm);

    // 2. Read file
    let read_tool = ReadFile;
    let res = read_tool
        .execute(
            &ctx,
            &json!({
                "path": "test.txt"
            }),
        )
        .await;
    assert_eq!(res.for_llm, "Hello World");

    // 3. List dir
    let list_tool = ListDir;
    let res = list_tool.execute(&ctx, &json!({})).await;
    assert!(res.for_llm.contains("test.txt"));
    assert!(res.for_llm.contains("memory")); // created by TestWorkspace
}

#[tokio::test]
async fn test_path_traversal() {
    let ws = TestWorkspace::new();
    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: None,
        channel: None,
        outbound_tx: None,
        delivered: Default::default(),
    };

    let read_tool = ReadFile;
    let res = read_tool
        .execute(
            &ctx,
            &json!({
                "path": "../../../etc/passwd"
            }),
        )
        .await;
    assert!(res.is_error);
    assert!(res.for_llm.contains("escape"));
}

// --- §3.3 Path traversal: write_file, edit_file, append_file, list_dir ---

fn ctx_restricted(workspace: &std::path::Path) -> ToolCtx {
    ToolCtx {
        workspace: workspace.to_path_buf(),
        restrict_to_workspace: true,
        chat_id: None,
        channel: None,
        outbound_tx: None,
        delivered: Default::default(),
    }
}

#[tokio::test]
async fn test_path_traversal_write_file() {
    let ws = TestWorkspace::new();
    let res = WriteFile
        .execute(
            &ctx_restricted(&ws.root),
            &json!({ "path": "../../../etc/passwd", "content": "x" }),
        )
        .await;
    assert!(res.is_error, "write_file must reject path traversal");
    assert!(
        res.for_llm.contains("escape") || res.for_llm.contains("restricted"),
        "error should mention escape or restriction: {}",
        res.for_llm
    );
}

#[tokio::test]
async fn test_path_traversal_edit_file() {
    let ws = TestWorkspace::new();
    let res = EditFile
        .execute(
            &ctx_restricted(&ws.root),
            &json!({
                "path": "../../../etc/passwd",
                "old_text": "x",
                "new_text": "y"
            }),
        )
        .await;
    assert!(res.is_error);
    assert!(
        res.for_llm.contains("escape") || res.for_llm.contains("restricted"),
        "{}",
        res.for_llm
    );
}

#[tokio::test]
async fn test_path_traversal_append_file() {
    let ws = TestWorkspace::new();
    let res = AppendFile
        .execute(
            &ctx_restricted(&ws.root),
            &json!({ "path": "../../../tmp/escape", "content": "x" }),
        )
        .await;
    assert!(res.is_error);
    assert!(
        res.for_llm.contains("escape") || res.for_llm.contains("restricted"),
        "{}",
        res.for_llm
    );
}

#[tokio::test]
async fn test_path_traversal_list_dir() {
    let ws = TestWorkspace::new();
    let res = ListDir
        .execute(
            &ctx_restricted(&ws.root),
            &json!({ "path": "../../../etc" }),
        )
        .await;
    assert!(res.is_error);
    assert!(
        res.for_llm.contains("escape") || res.for_llm.contains("restricted"),
        "{}",
        res.for_llm
    );
}

// --- §3.3 edit_file / append_file on missing file ---

#[tokio::test]
async fn test_edit_file_missing_file_returns_error() {
    let ws = TestWorkspace::new();
    let res = EditFile
        .execute(
            &ctx_restricted(&ws.root),
            &json!({
                "path": "nonexistent.txt",
                "old_text": "a",
                "new_text": "b"
            }),
        )
        .await;
    assert!(res.is_error);
    assert!(!res.for_llm.is_empty());
}

#[tokio::test]
async fn test_append_file_creates_missing_file() {
    let ws = TestWorkspace::new();
    let path = ws.root.join("new_note.txt");
    assert!(!path.exists());
    let res = AppendFile
        .execute(
            &ctx_restricted(&ws.root),
            &json!({ "path": "new_note.txt", "content": "first line\n" }),
        )
        .await;
    assert!(
        !res.is_error,
        "append_file should create then append: {}",
        res.for_llm
    );
    assert!(path.exists());
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "first line\n");
}

// --- §3.3 message tool delivers to outbound with correct chat_id ---

#[tokio::test]
async fn test_message_tool_sends_to_outbound() {
    use icrab::agent::process_message;
    use icrab::llm::HttpProvider;
    use icrab::tools::registry::ToolRegistry;
    use tokio::sync::mpsc;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    let ws = TestWorkspace::new();
    let mock_llm = common::MockLlm::new().await;
    let config = common::create_test_config(&ws.root, &mock_llm.endpoint());
    let provider = HttpProvider::from_config(&config).expect("provider");

    let registry = ToolRegistry::new();
    registry.register(ReadFile);
    registry.register(WriteFile);
    registry.register(MessageTool);

    let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
    let ctx = ToolCtx {
        workspace: ws.root.clone(),
        restrict_to_workspace: true,
        chat_id: Some(42),
        channel: Some("telegram".into()),
        outbound_tx: Some(std::sync::Arc::new(outbound_tx)),
        delivered: Default::default(),
    };

    // 1st call: LLM uses message tool
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Use message"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_msg",
                        "type": "function",
                        "function": {
                            "name": "message",
                            "arguments": "{\"text\": \"Hello from message tool\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_llm.server)
        .await;

    // 2nd call: final reply
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

    let db = std::sync::Arc::new(icrab::memory::db::BrainDb::open(&ws.root).unwrap());
    let _ = process_message(
        &provider,
        &registry,
        &ws.root,
        "gpt-4-test",
        "Europe/London",
        "chat_msg",
        "Use message tool to say Hello from message tool",
        &ctx,
        &db,
    )
    .await
    .expect("process_message should succeed");

    let out = outbound_rx.recv().await.expect("one outbound message");
    assert_eq!(out.chat_id, 42);
    assert_eq!(out.text, "Hello from message tool");
}

// --- §3.3 Web tools degrade gracefully (web_fetch with mock server) ---

#[tokio::test]
async fn test_web_fetch_500_returns_error_not_crash() {
    let mock_server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&mock_server)
        .await;

    let client = web_client().expect("web client");
    let tool = WebFetchTool::new(client, 1000);
    let ctx = ctx_restricted(std::path::Path::new("/tmp"));

    let res = tool
        .execute(&ctx, &json!({ "url": mock_server.uri().to_string() }))
        .await;

    // Tool should not crash; it may return is_error or a safe message to the LLM (status + body).
    assert!(
        res.is_error
            || res.for_llm.contains("500")
            || res.for_llm.contains("Internal Server Error"),
        "500 should yield error or safe status message: {}",
        res.for_llm
    );
    assert!(!res.for_llm.is_empty());
}

#[tokio::test]
async fn test_web_fetch_empty_body_returns_safely() {
    let mock_server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&mock_server)
        .await;

    let client = web_client().expect("web client");
    let tool = WebFetchTool::new(client, 1000);
    let ctx = ctx_restricted(std::path::Path::new("/tmp"));

    let res = tool
        .execute(&ctx, &json!({ "url": mock_server.uri().to_string() }))
        .await;

    assert!(!res.is_error);
    assert!(res.for_llm.is_empty() || res.for_llm.len() < 100);
}
