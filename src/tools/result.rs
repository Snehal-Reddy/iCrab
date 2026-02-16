//! Tool execution result: for_llm, for_user, silent, is_error, async.

/// Result of executing a tool: content for the LLM, optional user message, flags.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// Content appended to conversation for the LLM.
    pub for_llm: String,
    /// If present and not silent, send to user (e.g. Telegram).
    pub for_user: Option<String>,
    /// If true, do not send for_user to user even when set.
    pub silent: bool,
    /// If true, treat as tool error (LLM may retry or report).
    pub is_error: bool,
    /// If true, tool started async work; completion reported later (e.g. via message tool).
    #[allow(non_snake_case)]
    pub async_: bool,
}

impl ToolResult {
    /// Success: content for LLM only.
    #[inline]
    pub fn ok(for_llm: impl Into<String>) -> Self {
        Self {
            for_llm: for_llm.into(),
            for_user: None,
            silent: false,
            is_error: false,
            async_: false,
        }
    }

    /// User-facing message (sent to user unless silent).
    #[inline]
    pub fn user(content: impl Into<String>) -> Self {
        let s = content.into();
        Self {
            for_llm: s.clone(),
            for_user: Some(s),
            silent: false,
            is_error: false,
            async_: false,
        }
    }

    /// Silent success: for LLM only, do not send to user.
    #[inline]
    pub fn silent(for_llm: impl Into<String>) -> Self {
        Self {
            for_llm: for_llm.into(),
            for_user: None,
            silent: true,
            is_error: false,
            async_: false,
        }
    }

    /// Error: for_llm = msg, is_error = true.
    #[inline]
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            for_llm: msg.into(),
            for_user: None,
            silent: false,
            is_error: true,
            async_: false,
        }
    }

    /// Async: tool started background work; for_llm describes what was started.
    #[inline]
    pub fn async_(for_llm: impl Into<String>) -> Self {
        Self {
            for_llm: for_llm.into(),
            for_user: None,
            silent: false,
            is_error: false,
            async_: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_set_fields() {
        let r = ToolResult::ok("done");
        assert_eq!(r.for_llm, "done");
        assert!(r.for_user.is_none());
        assert!(!r.is_error);
        assert!(!r.async_);

        let r = ToolResult::error("failed");
        assert_eq!(r.for_llm, "failed");
        assert!(r.is_error);

        let r = ToolResult::async_("Subagent started");
        assert!(r.async_);
    }
}
