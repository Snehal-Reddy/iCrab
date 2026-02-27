//! Send text to user via outbound queue (Telegram); used by main agent and subagent.

use std::sync::atomic::Ordering;

use serde_json::Value;

use crate::telegram::OutboundMsg;
use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

fn get_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("missing or invalid '{key}'"))
}

/// message tool: send text to the current chat via outbound_tx.
pub struct MessageTool;

impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message"
    }

    fn description(&self) -> &str {
        "Send a text message to the user in the current chat (e.g. Telegram)."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Message text to send to user" }
            },
            "required": ["text"]
        })
    }

    fn execute<'a>(&'a self, ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let args = args.clone();
        let ctx = ctx.clone();

        Box::pin(async move {
            let text = match get_string(&args, "text") {
                Ok(t) => t,
                Err(e) => return ToolResult::error(e),
            };
            let Some(tx) = &ctx.outbound_tx else {
                return ToolResult::error("no outbound channel (message tool unavailable)");
            };
            let Some(chat_id) = ctx.chat_id else {
                return ToolResult::error("no chat_id (message tool unavailable)");
            };
            let channel = ctx
                .channel
                .clone()
                .unwrap_or_else(|| "telegram".to_string());
            let msg = OutboundMsg {
                chat_id,
                text,
                channel,
            };
            match tx.try_send(msg) {
                Ok(()) => {
                    ctx.delivered.store(true, Ordering::Relaxed);
                    ToolResult::silent("sent")
                }
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
}
