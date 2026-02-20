use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use serde_json::json;

use super::Sessions;

const MAX_BYTES: usize = 20 * 1024;

pub struct Tool {
    sessions: Sessions,
}

pub const fn new(sessions: Sessions) -> Tool {
    Tool { sessions }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    /// Optional CSS selector to get content of a specific element.
    selector: Option<String>,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("browser_content"),
            "Get the HTML content of the current page. Optionally pass a CSS selector to scope to a specific element. Max 20KB.",
        )
        .with_input_schema::<Input>()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn execute(
        &self,
        input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            tracing::info!(selector = ?input.selector, "browser_content");
            let session = self.sessions.session()?;

            let expression = input.selector.as_ref().map_or_else(
                || "document.documentElement.outerHTML".to_owned(),
                |sel| {
                    let sel = super::js_string_literal(sel);
                    format!(
                        "(function() {{ var el = document.querySelector({sel}); return el ? el.outerHTML : null; }})()"
                    )
                },
            );

            let result = session
                .send(
                    "Runtime.evaluate",
                    json!({
                        "expression": expression,
                        "returnByValue": true,
                    }),
                )
                .await?;

            if let Some(exception) = result.get("exceptionDetails") {
                let msg = exception
                    .get("exception")
                    .and_then(|e| e.get("description"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("content retrieval failed");
                return Err(msg.to_owned());
            }

            let html = result
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
                .ok_or("no content returned")?
                .to_owned();

            if html.len() <= MAX_BYTES {
                return Ok(html);
            }

            let boundary = html.floor_char_boundary(MAX_BYTES);
            let mut truncated = html[..boundary].to_owned();
            truncated.push_str("\n[Truncated at 20KB]");
            Ok(truncated)
        })
    }
}