use std::path::PathBuf;
use serde_json::json;
use icrab::tools::{self, ToolCtx};
use icrab::config::Config;

#[tokio::main]
async fn main() {
    println!("Debugging web tools...");

    // Mock config
    let config = Config {
        workspace: Some("/tmp/icrab-debug".to_string()),
        telegram: None,
        llm: None,
        tools: None, // This will default to DDG
        heartbeat: None,
        restrict_to_workspace: Some(true),
    };

    let registry = tools::build_core_registry(&config);
    
    println!("Registered tools: {:?}", registry.list());

    if !registry.list().contains(&"web_search".to_string()) {
        println!("ERROR: web_search tool not registered!");
        return;
    }

    let ctx = ToolCtx {
        workspace: PathBuf::from("/tmp/icrab-debug"),
        restrict_to_workspace: true,
        chat_id: Some(12345),
        channel: Some("debug".to_string()),
        outbound_tx: None,
    };

    println!("Executing web_search for 'Taylor Swift birthday'...");
    let args = json!({
        "query": "Taylor Swift birthday"
    });

    let result = registry.execute(&ctx, "web_search", &args).await;

    println!("Result is_error: {}", result.is_error);
    println!("Result for_llm: {}", result.for_llm);
    
    if let Some(user_text) = result.for_user {
        println!("Result for_user: {}", user_text);
    }
}
