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
    /// CSS selector of the input element.
    selector: String,
    /// Text to type into the element.
    text: String,
    /// Whether to press Enter after typing. Default: false.
    press_enter: Option<bool>,
}

#[derive(serde::Serialize)]
pub struct Output {
    typed: bool,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("browser_type"),
            "Type text into an input element. Optionally press Enter after.",
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
            tracing::info!(selector = %input.selector, "browser_type");
            let session = self.sessions.session()?;

            let sel = super::js_string_literal(&input.selector);

            // Focus the element.
            let focus_expr = format!(
                "(function() {{ var el = document.querySelector({sel}); if (!el) throw new Error('element not found: ' + {sel}); el.focus(); return true; }})()"
            );

            let result = session
                .send(
                    "Runtime.evaluate",
                    json!({
                        "expression": focus_expr,
                        "returnByValue": true,
                    }),
                )
                .await?;

            if let Some(exception) = result.get("exceptionDetails") {
                let msg = exception
                    .get("exception")
                    .and_then(|e| e.get("description"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("focus failed");
                return Err(msg.to_owned());
            }

            // Insert text via CDP Input domain.
            session
                .send("Input.insertText", json!({"text": input.text}))
                .await?;

            // Optionally press Enter.
            if input.press_enter.unwrap_or(false) {
                session
                    .send(
                        "Input.dispatchKeyEvent",
                        json!({
                            "type": "keyDown",
                            "key": "Enter",
                            "code": "Enter",
                            "windowsVirtualKeyCode": 13,
                            "nativeVirtualKeyCode": 13,
                        }),
                    )
                    .await?;
                session
                    .send(
                        "Input.dispatchKeyEvent",
                        json!({
                            "type": "keyUp",
                            "key": "Enter",
                            "code": "Enter",
                            "windowsVirtualKeyCode": 13,
                            "nativeVirtualKeyCode": 13,
                        }),
                    )
                    .await?;
            }

            Ok(Output { typed: true })
        })
    }
}
