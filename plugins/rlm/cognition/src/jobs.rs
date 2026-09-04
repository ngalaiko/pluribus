use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    #[serde(default)]
    pub activities: std::collections::BTreeMap<String, Value>,
    pub id: String,
    pub objective: String,
    pub completion_conditions: Value,
    pub origin: String,
    pub sources: Vec<String>,
    pub revision: u64,
    pub incorporated_sequence: u64,
    pub status: String,
    #[serde(default)]
    pub wait_reason: Option<String>,
    #[serde(default)]
    pub outstanding_question: Option<String>,
    #[serde(default)]
    pub recent_reply: Option<String>,
    pub completed_steps: Value,
    pub next_step: Value,
    pub blockers: Value,
    pub notes: Value,
    pub checkpoint: Value,
    pub checkpoint_version: u32,
    pub cycle_started_ms: i64,
    pub retry_count: u32,
    pub wake: Option<Wake>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Wake {
    pub token: String,
    pub request: Option<String>,
    pub revision: u64,
    pub due_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Observation {
    #[serde(default)]
    pub outstanding_question: Option<String>,
    pub id: String,
    pub sequence: u64,
    pub value: Value,
    pub status: String,
    pub job: Option<String>,
}
