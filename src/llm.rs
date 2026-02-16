//! LLM provider: `chat(messages, tools, model) -> (content, tool_calls)`.
//!
//! Single HTTP provider (OpenRouter default). No streaming; minimal types.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, LlmConfig};

// --- Types ---

/// Chat message role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single chat message (system/user/assistant or tool result).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Set for tool-result messages (role = Tool).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Set for assistant messages that requested tool calls (OpenAI shape).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// OpenAI-style function tool: `type: "function"`, `function: { name, description, parameters }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub type_: String,
    pub function: ToolFunctionDef,
}

/// Inner function definition for a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    /// JSON schema object, e.g. `{"type":"object","properties":{...}}`.
    pub parameters: serde_json::Value,
}

impl ToolDef {
    /// New function tool (OpenAI shape).
    pub fn function(name: String, description: String, parameters: serde_json::Value) -> Self {
        Self {
            type_: "function".to_string(),
            function: ToolFunctionDef {
                name,
                description,
                parameters,
            },
        }
    }
}

/// One tool call returned by the API (id, name, arguments JSON string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

/// Token usage (optional, for logging).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageInfo {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// LLM response: content, tool_calls, finish_reason, optional usage.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: Option<UsageInfo>,
}

/// LLM module errors.
#[derive(Debug)]
pub enum LlmError {
    Config(String),
    Http(String),
    Parse(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Config(s) => write!(f, "llm config: {}", s),
            LlmError::Http(s) => write!(f, "llm http: {}", s),
            LlmError::Parse(s) => write!(f, "llm parse: {}", s),
        }
    }
}

impl std::error::Error for LlmError {}

// --- Request/response (raw API shape for serde) ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDef]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Option<Vec<Choice>>,
    usage: Option<UsageInfo>,
}

#[derive(Deserialize)]
struct Choice {
    message: Option<ChoiceMessage>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
}

// --- Provider ---

/// HTTP provider (OpenRouter, OpenAI, Groq, etc.).
pub struct HttpProvider {
    api_base: String,
    api_key: String,
    client: reqwest::Client,
}

const DEFAULT_API_BASE: &str = "https://openrouter.ai/api/v1";
const REQUEST_TIMEOUT_SECS: u64 = 120;

impl HttpProvider {
    /// Build provider from validated config. Uses `cfg.llm`; default api_base is OpenRouter.
    pub fn from_config(cfg: &Config) -> Result<Self, LlmError> {
        let llm: &LlmConfig = cfg
            .llm
            .as_ref()
            .ok_or_else(|| LlmError::Config("llm section missing".into()))?;
        let api_key = llm
            .api_key
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| LlmError::Config("llm.api_key required".into()))?
            .to_string();
        let api_base = llm
            .api_base
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_API_BASE)
            .trim_end_matches('/')
            .to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| LlmError::Config(format!("reqwest client: {}", e)))?;
        Ok(Self {
            api_base,
            api_key,
            client,
        })
    }

    /// Send chat request; returns content and tool_calls. Empty choices yield empty content and no tool_calls.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        model: &str,
    ) -> Result<LlmResponse, LlmError> {
        let url = format!("{}/chat/completions", self.api_base);
        let (tools_param, tool_choice) = if tools.is_empty() {
            (None, None)
        } else {
            (Some(tools), Some("auto"))
        };
        let body = ChatRequest {
            model,
            messages,
            tools: tools_param,
            tool_choice,
        };
        let res = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Http(format!("{} {}", status, text)));
        }

        let parsed: ChatResponse =
            serde_json::from_str(&text).map_err(|e| LlmError::Parse(e.to_string()))?;

        let (content, tool_calls, finish_reason) = parsed
            .choices
            .as_deref()
            .and_then(|c| c.first())
            .and_then(|choice| {
                let msg = choice.message.as_ref()?;
                let content = msg
                    .content
                    .as_deref()
                    .unwrap_or("")
                    .to_string();
                let tool_calls = msg.tool_calls.clone().unwrap_or_default();
                let finish_reason = choice
                    .finish_reason
                    .as_deref()
                    .unwrap_or("")
                    .to_string();
                Some((content, tool_calls, finish_reason))
            })
            .unwrap_or_else(|| (String::new(), Vec::new(), String::new()));

        Ok(LlmResponse {
            content,
            tool_calls,
            finish_reason,
            usage: parsed.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_shape_no_tools() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "Hi".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        let body = ChatRequest {
            model: "gpt-4",
            messages: &messages,
            tools: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "gpt-4");
        assert!(json["messages"].is_array());
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "Hi");
        assert!(json.get("tools").is_none());
        assert!(json.get("tool_choice").is_none());
    }

    #[test]
    fn request_body_shape_with_tools() {
        let messages = vec![Message {
            role: Role::User,
            content: "Run foo".to_string(),
            tool_call_id: None,
            tool_calls: None,
        }];
        let tools = vec![ToolDef::function(
            "foo".to_string(),
            "Run foo".to_string(),
            serde_json::json!({"type":"object","properties":{}}),
        )];
        let body = ChatRequest {
            model: "gpt-4",
            messages: &messages,
            tools: Some(&tools),
            tool_choice: Some("auto"),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "foo");
        assert_eq!(json["tool_choice"], "auto");
    }

    #[test]
    fn request_body_assistant_with_tool_calls() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    type_: "function".to_string(),
                    function: ToolCallFunction {
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"x"}"#.to_string(),
                    },
                }]),
            },
        ];
        let body = ChatRequest {
            model: "gpt-4",
            messages: &messages,
            tools: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        let msg = &json["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert!(msg["tool_calls"].is_array());
        assert_eq!(msg["tool_calls"][0]["id"], "call_1");
        assert_eq!(msg["tool_calls"][0]["type"], "function");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(msg["tool_calls"][0]["function"]["arguments"], r#"{"path":"x"}"#);
    }
}
