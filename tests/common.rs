use std::path::{Path, PathBuf};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use icrab::config::{Config, LlmConfig, TelegramConfig, ToolsConfig, WebConfig};

pub struct TestWorkspace {
    // Keep TempDir alive so dir isn't deleted until struct drop
    _tmp: TempDir,
    pub root: PathBuf,
}

impl TestWorkspace {
    pub fn new() -> Self {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let root = tmp.path().to_path_buf();

        // Create standard dirs
        std::fs::create_dir_all(root.join("memory")).unwrap();
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::create_dir_all(root.join("skills")).unwrap();

        // Create empty MEMORY.md
        std::fs::write(root.join("memory/MEMORY.md"), "").unwrap();

        Self { _tmp: tmp, root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
}

pub struct MockLlm {
    pub server: MockServer,
}

impl MockLlm {
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        Self { server }
    }

    pub fn endpoint(&self) -> String {
        self.server.uri()
    }

    /// Mount a mock for /chat/completions that returns the given JSON body.
    pub async fn mock_chat_completion(&self, response_body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&self.server)
            .await;
    }
}

pub fn create_test_config(workspace: &Path, llm_endpoint: &str) -> Config {
    Config {
        workspace: Some(workspace.to_string_lossy().to_string()),
        telegram: Some(TelegramConfig {
            bot_token: Some("test_token".to_string()),
            allowed_user_ids: Some(vec![12345]),
        }),
        llm: Some(LlmConfig {
            provider: Some("openai".to_string()), // or openrouter
            api_base: Some(llm_endpoint.to_string()),
            api_key: Some("test_key".to_string()),
            model: Some("gpt-4-test".to_string()),
        }),
        tools: Some(ToolsConfig {
            web: Some(WebConfig {
                brave_api_key: Some("test_brave_key".to_string()),
                brave_max_results: Some(5),
                web_fetch_max_chars: Some(1000),
            }),
        }),
        heartbeat: None,
        restrict_to_workspace: Some(true),
    }
}
