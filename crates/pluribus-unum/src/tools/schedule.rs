mod delete;
mod list;
mod set;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use pluribus_frequency::state::Configuration;

use super::{Provider, Tools};

const SCHEDULE_PREFIX: &str = "schedule.";

/// When a schedule should fire.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Trigger {
    /// Fire once at a specific time, then auto-delete.
    Once { at: DateTime<Utc> },
    /// Fire on a recurring cron schedule (stored as the expression string).
    Cron { cron: String },
}

/// A persisted schedule entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Schedule {
    /// Prompt injected as a user message when the schedule fires.
    pub prompt: String,
    /// When to fire.
    pub trigger: Trigger,
}

pub struct Scheduler;

impl Provider for Scheduler {
    fn resolve(&self, config: &Configuration) -> Tools {
        let mut tools = Tools::new();
        tools.register(set::new(config.clone()));
        tools.register(delete::new(config.clone()));
        tools.register(list::new(config.clone()));
        tools
    }
}

/// Read all schedule entries from configuration.
#[must_use]
pub fn entries(config: &Configuration) -> HashMap<String, Schedule> {
    config
        .list_by_prefix::<Schedule>(SCHEDULE_PREFIX)
        .into_iter()
        .map(|(k, v)| (k[SCHEDULE_PREFIX.len()..].to_owned(), v))
        .collect()
}

/// Full configuration key for a schedule id.
fn config_key(id: &str) -> String {
    format!("{SCHEDULE_PREFIX}{id}")
}
