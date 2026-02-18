//! web_search (Brave/DDG), web_fetch (GET URL, truncated body).

use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;

const USER_AGENT: &str = "iCrab/1.0 (https://github.com/Snehal-Reddy/iCrab)";
const SEARCH_TIMEOUT_SECS: u64 = 15;
const FETCH_TIMEOUT_SECS: u64 = 60;
const MAX_REDIRECTS: u32 = 5;

/// Search provider: Brave API or DuckDuckGo HTML fallback.
#[derive(Clone)]
pub enum WebSearchProvider {
    Brave { api_key: String, max_results: u8 },
    DuckDuckGo { max_results: u8 },
}

impl WebSearchProvider {
    fn max_results(&self) -> u8 {
        match self {
            Self::Brave { max_results, .. } => *max_results,
            Self::DuckDuckGo { max_results } => *max_results,
        }
    }

    async fn search(&self, client: &Client, query: &str, count: u8) -> Result<String, String> {
        let count = count.clamp(1, 10);
        match self {
            Self::Brave { api_key, .. } => brave_search(client, api_key, query, count).await,
            Self::DuckDuckGo { .. } => duckduckgo_search(client, query, count).await,
        }
    }
}

async fn brave_search(
    client: &Client,
    api_key: &str,
    query: &str,
    count: u8,
) -> Result<String, String> {
    let url = reqwest::Url::parse_with_params(
        "https://api.search.brave.com/res/v1/web/search",
        &[("q", query), ("count", &count.to_string())],
    )
    .map_err(|e| e.to_string())?;
    let res = client
        .get(url)
        .header("X-Subscription-Token", api_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    let body = res.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Brave API error {}: {}", status, body.trim()));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let results = v
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    Ok(format_brave_results(results))
}

/// Format Brave API web.results array for LLM. Used by brave_search and tests.
fn format_brave_results(results: &[Value]) -> String {
    let mut lines = Vec::with_capacity(results.len());
    for r in results {
        let title = r.get("title").and_then(Value::as_str).unwrap_or("");
        let url = r.get("url").and_then(Value::as_str).unwrap_or("");
        let desc = r.get("description").and_then(Value::as_str).unwrap_or("");
        lines.push(format!("- **{}**\n  {}\n  {}", title, url, desc));
    }
    if lines.is_empty() {
        "No results.".to_string()
    } else {
        lines.join("\n\n")
    }
}

async fn duckduckgo_search(client: &Client, query: &str, count: u8) -> Result<String, String> {
    let url = reqwest::Url::parse_with_params("https://html.duckduckgo.com/html/", &[("q", query)])
        .map_err(|e| e.to_string())?;
    let res = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("DuckDuckGo returned {}", res.status()));
    }
    let html = res.text().await.map_err(|e| e.to_string())?;
    extract_ddg_results(&html, count)
}

/// Extract result links and optional snippets from DDG HTML (regex-based).
fn extract_ddg_results(html: &str, max: u8) -> Result<String, String> {
    // DDG HTML: result links in <a class="result__a" href="...">title</a>, snippet in result__snippet.
    let link_re = Regex::new(r#"<a\s+class="result__a"[^>]*href="([^"]+)"[^>]*>([^<]*)</a>"#)
        .map_err(|e| e.to_string())?;
    let snippet_re = Regex::new(
        r#"<a\s+class="result__a"[^>]*href="([^"]+)"[^>]*>([^<]*)</a>(?:\s*<div[^>]*>)*\s*<div[^>]*class="[^"]*result__snippet[^"]*"[^>]*>([^<]*)</div>"#,
    )
    .map_err(|e| e.to_string())?;

    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Prefer snippet matches (title + url + snippet)
    for cap in snippet_re.captures_iter(html) {
        if lines.len() >= max as usize {
            break;
        }
        let url = html_unescape(&cap[1]);
        let title = html_unescape(&cap[2]).trim().to_string();
        let snippet = html_unescape(&cap[3]).trim().to_string();
        if seen.insert(url.clone()) {
            lines.push(format!("- **{}**\n  {}\n  {}", title, url, snippet));
        }
    }
    // Then links without snippet
    if lines.len() < max as usize {
        for cap in link_re.captures_iter(html) {
            if lines.len() >= max as usize {
                break;
            }
            let url = html_unescape(&cap[1]);
            let title = html_unescape(&cap[2]).trim().to_string();
            if seen.insert(url.clone()) {
                lines.push(format!("- **{}**\n  {}", title, url));
            }
        }
    }
    if lines.is_empty() {
        return Ok("No results.".to_string());
    }
    Ok(lines.join("\n\n"))
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Strip script/style, then tags; collapse whitespace.
pub fn html_to_text(html: &str) -> String {
    let script_re = Regex::new(r"(?s)<script[^>]*>.*?</script>").unwrap();
    let style_re = Regex::new(r"(?s)<style[^>]*>.*?</style>").unwrap();
    let tag_re = Regex::new("<[^>]+>").unwrap();
    let space_re = Regex::new(r"\s+").unwrap();

    let s = script_re.replace_all(html, " ");
    let s = style_re.replace_all(&s, " ");
    let s = tag_re.replace_all(&s, " ");
    let s = space_re.replace_all(&s, " ");
    s.trim().to_string()
}

fn get_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("missing or invalid '{key}'"))
}

fn get_optional_u8(args: &Value, key: &str) -> Option<u8> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u8::try_from(n).ok())
}

fn get_optional_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// web_search tool: Brave API or DuckDuckGo; returns titles, URLs, snippets.
pub struct WebSearchTool {
    pub provider: WebSearchProvider,
    pub client: Client,
}

impl WebSearchTool {
    pub fn new(provider: WebSearchProvider, client: Client) -> Self {
        Self { provider, client }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Returns titles, URLs, and snippets. Use for finding current info or links."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "integer", "description": "Max results 1-10", "minimum": 1, "maximum": 10 }
            },
            "required": ["query"]
        })
    }

    fn execute<'a>(&'a self, _ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let args = args.clone();
        let provider = self.provider.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let query = match get_string(&args, "query") {
                Ok(q) => q,
                Err(e) => return ToolResult::error(e),
            };
            let count = get_optional_u8(&args, "count")
                .unwrap_or_else(|| provider.max_results())
                .clamp(1, 10);
            match provider.search(&client, &query, count).await {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::error(e),
            }
        })
    }
}

/// Validate URL: http/https and has host.
fn validate_fetch_url(s: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(s).map_err(|e| e.to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("only http and https URLs are allowed".to_string());
    }
    if url.host_str().is_none() {
        return Err("URL must have a host".to_string());
    }
    Ok(url)
}

/// web_fetch tool: GET URL, return body as text (JSON pretty, HTML stripped, truncated).
pub struct WebFetchTool {
    pub client: Client,
    pub max_chars: u32,
}

impl WebFetchTool {
    pub fn new(client: Client, max_chars: u32) -> Self {
        Self { client, max_chars }
    }
}

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "GET a URL and return its body as text (for summarization). HTML is converted to text; JSON is pretty-printed. Result is truncated to max_chars."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch (http or https)" },
                "max_chars": { "type": "integer", "description": "Optional max characters to return" }
            },
            "required": ["url"]
        })
    }

    fn execute<'a>(&'a self, _ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let args = args.clone();
        let client = self.client.clone();
        let max_chars = self.max_chars;
        Box::pin(async move {
            let url_str = match get_string(&args, "url") {
                Ok(u) => u,
                Err(e) => return ToolResult::error(e),
            };
            let url = match validate_fetch_url(&url_str) {
                Ok(u) => u,
                Err(e) => return ToolResult::error(e),
            };
            let max_chars = get_optional_u32(&args, "max_chars").unwrap_or(max_chars);

            let res = match client.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) => return ToolResult::error(e.to_string()),
            };
            let status = res.status();
            let headers = res.headers().clone();
            let body = match res.bytes().await {
                Ok(b) => b,
                Err(e) => return ToolResult::error(e.to_string()),
            };

            let content_type = headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_lowercase();

            let text = if content_type.contains("application/json") {
                match serde_json::from_slice::<Value>(&body) {
                    Ok(v) => serde_json::to_string_pretty(&v)
                        .unwrap_or_else(|_| String::from_utf8_lossy(&body).into_owned()),
                    Err(_) => String::from_utf8_lossy(&body).into_owned(),
                }
            } else if content_type.contains("text/html")
                || content_type.contains("application/xhtml")
            {
                let raw = String::from_utf8_lossy(&body).into_owned();
                html_to_text(&raw)
            } else {
                String::from_utf8_lossy(&body).into_owned()
            };

            let truncated = text.len() > max_chars as usize;
            let out = if truncated {
                format!(
                    "[Truncated to {} chars]\n\n{}",
                    max_chars,
                    &text[..max_chars as usize]
                )
            } else {
                text
            };

            let header = format!(
                "URL: {}\nStatus: {}\nLength: {} bytes{}\n\n",
                url,
                status,
                body.len(),
                if truncated {
                    format!(" (truncated to {} chars)", max_chars)
                } else {
                    String::new()
                }
            );
            ToolResult::ok(format!("{header}{out}"))
        })
    }
}

/// Build a HTTP client for web tools (timeouts, redirect limit, User-Agent).
pub fn web_client() -> Result<Client, String> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .connect_timeout(std::time::Duration::from_secs(SEARCH_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS as usize))
        .build()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_ctx() -> ToolCtx {
        ToolCtx {
            workspace: PathBuf::from("/tmp"),
            restrict_to_workspace: true,
            chat_id: None,
            channel: None,
            outbound_tx: None,
        }
    }

    #[test]
    fn html_to_text_strips_script_style_and_tags() {
        let html = "<html><head><script>alert(1)</script><style>.x{}</style></head><body><p>Hello</p>  <b>world</b></body></html>";
        let t = html_to_text(html);
        assert!(!t.contains("alert"));
        assert!(!t.contains(".x"));
        assert!(t.contains("Hello"));
        assert!(t.contains("world"));
    }

    #[test]
    fn html_to_text_collapses_whitespace() {
        let html = "<p>One</p>   <p>Two</p>\n\n<p>Three</p>";
        let t = html_to_text(html);
        assert!(t.contains("One"));
        assert!(t.contains("Two"));
        assert!(t.contains("Three"));
        // Single spaces between words, no raw newlines from tags
        assert!(!t.contains("   "));
    }

    #[test]
    fn html_to_text_empty_after_strip() {
        let html = "<script>only script</script><style>only style</style>";
        let t = html_to_text(html);
        assert!(t.trim().is_empty() || t.len() < 20);
    }

    #[test]
    fn html_unescape_entities() {
        assert_eq!(html_unescape("&amp;"), "&");
        assert_eq!(html_unescape("&lt;&gt;"), "<>");
        assert_eq!(html_unescape("&quot;x&#39;y"), "\"x'y");
        assert_eq!(html_unescape("a&nbsp;b"), "a b");
        assert_eq!(html_unescape("no entities"), "no entities");
    }

    #[test]
    fn validate_fetch_url_rejects_non_http() {
        assert!(validate_fetch_url("ftp://example.com").is_err());
        assert!(validate_fetch_url("file:///etc/passwd").is_err());
        assert!(validate_fetch_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn validate_fetch_url_rejects_no_host() {
        assert!(validate_fetch_url("https://").is_err());
        assert!(validate_fetch_url("http://").is_err());
    }

    #[test]
    fn validate_fetch_url_accepts_http_https() {
        assert!(validate_fetch_url("https://example.com/path").is_ok());
        assert!(validate_fetch_url("http://a.b.c:8080/").is_ok());
    }

    #[test]
    fn get_string_missing_key() {
        let args = serde_json::json!({});
        assert!(get_string(&args, "query").is_err());
    }

    #[test]
    fn get_string_valid() {
        let args = serde_json::json!({ "query": "test" });
        assert_eq!(get_string(&args, "query").unwrap(), "test");
    }

    #[test]
    fn format_brave_results_empty() {
        let results: &[Value] = &[];
        assert_eq!(format_brave_results(results), "No results.");
    }

    #[test]
    fn format_brave_results_one() {
        let results = [serde_json::json!({
            "title": "Example",
            "url": "https://example.com",
            "description": "An example site."
        })];
        let s = format_brave_results(&results);
        assert!(s.contains("**Example**"));
        assert!(s.contains("https://example.com"));
        assert!(s.contains("An example site."));
    }

    #[test]
    fn format_brave_results_missing_fields() {
        let results = [serde_json::json!({ "url": "https://u.org" })];
        let s = format_brave_results(&results);
        assert!(s.contains("https://u.org"));
    }

    #[test]
    fn extract_ddg_results_empty() {
        let html = "<html><body>no results here</body></html>";
        let out = extract_ddg_results(html, 5).unwrap();
        assert_eq!(out, "No results.");
    }

    #[test]
    fn extract_ddg_results_one_link() {
        let html = r#"<a class="result__a" href="https://example.com">Example</a>"#;
        let out = extract_ddg_results(html, 5).unwrap();
        assert!(out.contains("**Example**"));
        assert!(out.contains("https://example.com"));
    }

    #[test]
    fn extract_ddg_results_link_and_snippet() {
        let html = r#"<a class="result__a" href="https://a.com">Title</a><div class="result__snippet">Snippet text</div>"#;
        let out = extract_ddg_results(html, 5).unwrap();
        assert!(out.contains("**Title**"));
        assert!(out.contains("https://a.com"));
        assert!(out.contains("Snippet text"));
    }

    #[test]
    fn extract_ddg_results_respects_max() {
        let link = r#"<a class="result__a" href="https://u1.com">U1</a>"#;
        let html = format!("{0}{0}{0}{0}{0}{0}", link);
        let out = extract_ddg_results(&html, 3).unwrap();
        let count = out.matches("**U1**").count();
        assert!(count <= 3, "expected at most 3 results, got {}", count);
    }

    #[test]
    fn extract_ddg_results_unescapes_html() {
        let html = r#"<a class="result__a" href="https://a.com?a=1&amp;b=2">Foo &lt;bar&gt;</a>"#;
        let out = extract_ddg_results(html, 5).unwrap();
        assert!(out.contains("a=1&b=2"));
        assert!(out.contains("Foo <bar>"));
    }

    #[tokio::test]
    async fn web_search_tool_missing_query_returns_error() {
        let client = web_client().expect("client");
        let provider = WebSearchProvider::DuckDuckGo { max_results: 5 };
        let tool = WebSearchTool::new(provider, client);
        let ctx = dummy_ctx();
        let args = serde_json::json!({});
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("query") || res.for_llm.contains("missing"));
    }

    #[tokio::test]
    async fn web_fetch_tool_missing_url_returns_error() {
        let client = web_client().expect("client");
        let tool = WebFetchTool::new(client, 50_000);
        let ctx = dummy_ctx();
        let args = serde_json::json!({});
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("url") || res.for_llm.contains("missing"));
    }

    #[tokio::test]
    async fn web_fetch_tool_unsupported_scheme_returns_error() {
        let client = web_client().expect("client");
        let tool = WebFetchTool::new(client, 50_000);
        let ctx = dummy_ctx();
        let args = serde_json::json!({ "url": "ftp://example.com/file" });
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("http") || res.for_llm.contains("https"));
    }

    #[tokio::test]
    async fn web_fetch_tool_no_host_returns_error() {
        let client = web_client().expect("client");
        let tool = WebFetchTool::new(client, 50_000);
        let ctx = dummy_ctx();
        let args = serde_json::json!({ "url": "https://" });
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("host") || res.for_llm.to_lowercase().contains("url"));
    }

    #[test]
    fn web_search_tool_name_and_params() {
        let client = web_client().expect("client");
        let provider = WebSearchProvider::DuckDuckGo { max_results: 5 };
        let tool = WebSearchTool::new(provider, client);
        assert_eq!(tool.name(), "web_search");
        let params = tool.parameters();
        assert!(
            params
                .get("required")
                .and_then(|r| r.as_array())
                .unwrap()
                .contains(&serde_json::json!("query"))
        );
    }

    #[test]
    fn web_fetch_tool_name_and_params() {
        let client = web_client().expect("client");
        let tool = WebFetchTool::new(client, 10_000);
        assert_eq!(tool.name(), "web_fetch");
        let params = tool.parameters();
        assert!(
            params
                .get("required")
                .and_then(|r| r.as_array())
                .unwrap()
                .contains(&serde_json::json!("url"))
        );
    }
}
