use std::str::FromStr;

use chrono::DateTime;
use pluribus_frequency::state::Configuration;

use super::{config_key, Schedule, Trigger};

pub struct Tool {
    config: Configuration,
}

pub const fn new(config: Configuration) -> Tool {
    Tool { config }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    /// Short identifier for this schedule (e.g. "morning-reminder").
    id: String,
    /// The prompt to inject as a user message when the schedule fires.
    prompt: String,
    /// Fire once at this time (RFC 3339). Mutually exclusive with `cron`.
    #[serde(default)]
    at: Option<String>,
    /// Fire on a recurring cron schedule (5-field expression). Mutually exclusive with `at`.
    #[serde(default)]
    cron: Option<String>,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = String;

    fn def(&self) -> pluribus_frequency::protocol::ToolDef {
        pluribus_frequency::protocol::ToolDef::new(
            pluribus_frequency::protocol::ToolName::new("schedule_set"),
            "Create or update a schedule. Provide exactly one of `at` (RFC 3339 datetime for one-shot) or `cron` (5-field cron expression for recurring).",
        )
    }

    fn execute(
        &self,
        input: Input,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(async move {
            let trigger = match (input.at, input.cron) {
                (Some(at), None) => {
                    let dt = DateTime::parse_from_rfc3339(&at)
                        .map_err(|e| format!("invalid `at` datetime: {e}"))?;
                    let utc = dt.to_utc();
                    if utc < chrono::Utc::now() {
                        return Err("cannot schedule a one-shot trigger in the past".into());
                    }
                    Trigger::Once { at: utc }
                }
                (None, Some(cron_expr)) => {
                    // Validate the expression parses.
                    croner::Cron::from_str(&cron_expr)
                        .map_err(|e| format!("invalid cron expression: {e}"))?;
                    Trigger::Cron { cron: cron_expr }
                }
                (Some(_), Some(_)) => {
                    return Err("provide exactly one of `at` or `cron`, not both".into());
                }
                (None, None) => {
                    return Err("provide exactly one of `at` or `cron`".into());
                }
            };

            let schedule = Schedule {
                prompt: input.prompt,
                trigger,
            };

            self.config
                .set(&config_key(&input.id), &schedule)
                .map_err(|e| e.to_string())?;

            Ok(format!("schedule '{}' saved", input.id))
        })
    }
}
