use serde_json::json;

use icrab::tools::context::ToolCtx;
use icrab::tools::file::{ListDir, ReadFile, WriteFile};
use icrab::tools::registry::Tool;

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
