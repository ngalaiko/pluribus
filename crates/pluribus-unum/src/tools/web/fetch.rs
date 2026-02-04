use std::collections::HashMap;
use std::time::Duration;

use isahc::config::RedirectPolicy;
use isahc::prelude::*;
use isahc::Request;

const MAX_BYTES: usize = 20 * 1024;

pub struct Tool;

pub const fn new() -> Tool {
    Tool {}
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    pub url: String,
    pub headers: HashMap<String, String>,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = String;

    fn def(&self) -> pluribus_frequency::protocol::ToolDef {
        pluribus_frequency::protocol::ToolDef::new(
            pluribus_frequency::protocol::ToolName::new("web_fetch"),
            "Fetch a URL and return its text content (HTML parsed, scripts/styles removed). Max 20KB.",
        )
    }

    fn execute(
        &self,
        input: Input,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(fetch_url(input))
    }
}

fn is_html(response: &isahc::Response<isahc::AsyncBody>) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/html"))
}

/// Convert HTML to readable plain text using `dom_smoothie` (Mozilla readability).
/// Returns the raw HTML as-is if readability extraction fails.
fn html_to_text(html: &str, url: Option<&str>) -> String {
    let config = dom_smoothie::Config {
        text_mode: dom_smoothie::TextMode::Formatted,
        ..Default::default()
    };
    let Ok(mut readability) = dom_smoothie::Readability::new(html, url, Some(config)) else {
        return html.to_string();
    };
    match readability.parse() {
        Ok(article) => article.text_content.to_string(),
        Err(_) => html.to_string(),
    }
}

/// Find the largest valid UTF-8 char boundary at or before `index`.
const fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

async fn fetch_url(input: Input) -> Result<String, String> {
    let mut builder = Request::get(&input.url)
        .timeout(Duration::from_secs(15))
        .redirect_policy(RedirectPolicy::Limit(5))
        .header("User-Agent", "Pluribus/1.0")
        .header("Accept", "text/markdown, text/plain;q=0.9, text/html;q=0.8");

    for (key, value) in &input.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    let request = builder.body(()).map_err(|e| e.to_string())?;

    let client = isahc::HttpClient::new().map_err(|e| e.to_string())?;
    let mut response = client
        .send_async(request)
        .await
        .map_err(|e| e.to_string())?;

    let html = is_html(&response);
    let body = response.text().await.map_err(|e| e.to_string())?;

    let text = if html {
        html_to_text(&body, Some(&input.url))
    } else {
        body
    };

    if text.len() <= MAX_BYTES {
        return Ok(text);
    }

    let boundary = floor_char_boundary(&text, MAX_BYTES);
    let mut truncated = text[..boundary].to_string();
    truncated.push_str("\n[Truncated at 20KB]");
    Ok(truncated)
}
