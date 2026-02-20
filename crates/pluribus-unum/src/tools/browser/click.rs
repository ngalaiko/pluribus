use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use serde_json::json;

use super::Sessions;

pub struct Tool {
    sessions: Sessions,
}

pub const fn new(sessions: Sessions) -> Tool {
    Tool { sessions }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    /// CSS selector of the element to click.
    selector: String,
}

#[derive(serde::Serialize)]
pub struct Output {
    clicked: bool,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("browser_click"),
            "Click an element on the page by CSS selector.",
        )
        .with_input_schema::<Input>()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn execute(
        &self,
        input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send + '_>> {
        Box::pin(async move {
            tracing::info!(selector = %input.selector, "browser_click");
            let session = self.sessions.session()?;

            let sel = super::js_string_literal(&input.selector);

            // Get the element's bounding rectangle so we can compute its
            // center for a real mouse click.
            let expression = format!(
                "(function() {{ var el = document.querySelector({sel}); if (!el) throw new Error('element not found: ' + {sel}); var r = el.getBoundingClientRect(); return {{x: r.x + r.width / 2, y: r.y + r.height / 2}}; }})()"
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
                    .unwrap_or("click failed");
                return Err(msg.to_owned());
            }

            let coords = result
                .get("result")
                .and_then(|r| r.get("value"))
                .ok_or("failed to get element coordinates")?;
            let x = coords
                .get("x")
                .and_then(serde_json::Value::as_f64)
                .ok_or("missing x coordinate")?;
            let y = coords
                .get("y")
                .and_then(serde_json::Value::as_f64)
                .ok_or("missing y coordinate")?;

            // Simulate a real mouse click via CDP Input events.
            session
                .send(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mousePressed",
                        "x": x,
                        "y": y,
                        "button": "left",
                        "clickCount": 1,
                    }),
                )
                .await?;
            session
                .send(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseReleased",
                        "x": x,
                        "y": y,
                        "button": "left",
                        "clickCount": 1,
                    }),
                )
                .await?;

            Ok(Output { clicked: true })
        })
    }
}
