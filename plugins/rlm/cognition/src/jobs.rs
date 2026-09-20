use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn activity_result(event_id: &str, value: &Value) -> Value {
    if serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= 1024) {
        return value.clone();
    }
    let mut reference = serde_json::json!({"eventId":event_id,"inline":false});
    for field in ["code", "outcome"] {
        if let Some(text) = value[field].as_str().filter(|text| text.len() <= 128) {
            reference[field] = Value::String(text.to_owned());
        }
    }
    reference
}

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

#[cfg(test)]
mod tests {
    use crate::engine::{Config, Engine};
    use serde_json::{Value, json};

    #[test]
    fn activity_references_preserve_uncertain_outcomes_and_small_receipts() {
        let uncertain =
            json!({"code":"outcome-unknown", "outcome":"unknown", "output":"x".repeat(4096)});
        let reference = super::activity_result("event", &uncertain);
        assert_eq!(reference["code"], "outcome-unknown");
        assert_eq!(reference["outcome"], "unknown");
        assert!(reference.get("output").is_none());
        let receipt = json!({"output":{"exit_code":0}, "requestEventId":"request"});
        assert_eq!(super::activity_result("event", &receipt), receipt);
    }

    #[test]
    fn repeated_large_results_keep_job_snapshots_bounded() {
        let mut engine = Engine::default();
        let config = Config {
            tools: vec![],
            components: vec![],
        };
        engine.event(
            &config,
            "origin",
            "observation.received",
            &json!({
                "provider":"telegram", "externalSenderId":"user",
                "conversationId":"chat", "message":{"text":"work"}
            }),
            None,
        );
        for index in 0..32 {
            let request = format!("request-{index}");
            let result = format!("result-{index}");
            engine.event(
                &config,
                &request,
                "capability.requested",
                &json!({
                    "jobId":"origin", "revision":0, "capability":"shell.execute"
                }),
                None,
            );
            engine.event(
                &config,
                &result,
                "capability.completed",
                &json!({
                    "requestEventId":request, "output":{"stdout":"x".repeat(8192)}
                }),
                None,
            );
        }
        let job = &engine.jobs["origin"];
        assert!(
            serde_json::to_vec(job).unwrap().len() < 20 * 1024,
            "job snapshots duplicate full activity results"
        );
        assert_eq!(
            job.activities["request-31"]["result"]["eventId"],
            "result-31"
        );
        assert_eq!(job.activities["request-31"]["result"]["inline"], false);
        let restored: Engine =
            serde_json::from_value(serde_json::to_value(&engine).unwrap()).unwrap();
        assert_eq!(restored.jobs["origin"].activities, job.activities);
        assert!(restored.result_seen(
            "capability.completed",
            "result-31",
            &json!({"requestEventId":"request-31"})
        ));
        assert_eq!(job.checkpoint, Value::Null);
    }
}
