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
    /// JavaScript expression to evaluate.
    expression: String,
    /// Whether the expression returns a Promise that should be awaited. Default: false.
    await_promise: Option<bool>,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = serde_json::Value;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("browser_evaluate"),
            "Evaluate a JavaScript expression in the browser and return the result.",
        )
        .with_input_schema::<Input>()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn execute(
        &self,
        input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + '_>> {
        Box::pin(async move {
            tracing::info!("browser_evaluate");
            let session = self.sessions.session()?;

            let result = session
                .send(
                    "Runtime.evaluate",
                    json!({
                        "expression": input.expression,
                        "returnByValue": true,
                        "awaitPromise": input.await_promise.unwrap_or(false),
                    }),
                )
                .await?;

            if let Some(exception) = result.get("exceptionDetails") {
                let msg = exception
                    .get("exception")
                    .and_then(|e| e.get("description"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("evaluation failed");
                return Err(msg.to_owned());
            }

            let value = result
                .get("result")
                .and_then(|r| r.get("value"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            Ok(value)
        })
    }
}
