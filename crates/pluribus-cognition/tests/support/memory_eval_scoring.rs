use serde_json::{Value, json};

#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Transitions {
    pub requires_compaction: bool,
    pub compacted: bool,
    pub requires_restart: bool,
    pub restarted: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn score_case(
    id: &str,
    reply: &str,
    expected: &str,
    stale: &[&str],
    source_ids: &[String],
    transitions: Transitions,
    latency_ms: u64,
    completions: &[Value],
) -> Value {
    let parsed = serde_json::from_str::<Value>(reply).unwrap_or(Value::Null);
    let answer = parsed["answer"].as_str();
    let normalized = answer.map(|text| text.trim().to_lowercase());
    let accurate = normalized.as_deref() == Some(expected.trim().to_lowercase().as_str());
    let accuracy = f64::from(accurate);
    let stale_answer = normalized
        .as_ref()
        .is_some_and(|answer| stale.iter().any(|old| answer == &old.trim().to_lowercase()));
    let supplied = parsed["sourceIds"].as_array().and_then(|ids| {
        ids.iter()
            .map(Value::as_str)
            .collect::<Option<std::collections::BTreeSet<_>>>()
    });
    let expected_sources = source_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let valid_sources = supplied
        .as_ref()
        .is_some_and(|ids| !ids.is_empty() && ids == &expected_sources);
    let transitions_valid = (!transitions.requires_compaction || transitions.compacted)
        && (!transitions.requires_restart || transitions.restarted);
    let sum_tokens = |field: &str| -> Option<u64> {
        if completions.is_empty() {
            return None;
        }
        completions.iter().try_fold(0_u64, |total, completion| {
            total.checked_add(completion["usage"][field].as_u64()?)
        })
    };
    let cost = if completions.is_empty() {
        None
    } else {
        completions.iter().try_fold(0.0_f64, |total, completion| {
            let cost = completion
                .pointer("/usage/provider_metadata/cost")
                .or_else(|| completion.pointer("/provider_metadata/cost"))
                .and_then(Value::as_f64)?;
            let sum = total + cost;
            (cost >= 0.0 && sum.is_finite()).then_some(sum)
        })
    };
    json!({
        "id":id,
        "accuracy":accuracy,
        "staleFactResistance":f64::from(answer.is_some() && !stale_answer),
        "sourceValidity":f64::from(valid_sources),
        "valid":accurate && !stale_answer && valid_sources && transitions_valid,
        "transitionsValid":transitions_valid,
        "compacted":transitions.compacted,
        "restarted":transitions.restarted,
        "latencyMs":latency_ms,
        "modelCalls":completions.len(),
        "usage":{
            "inputTokens":sum_tokens("input_tokens"),
            "outputTokens":sum_tokens("output_tokens"),
            "reasoningTokens":sum_tokens("reasoning_tokens"),
            "cachedInputTokens":sum_tokens("cached_input_tokens"),
            "cost":cost
        },
        "answer":parsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(reply: &str, transitions: Transitions, completions: &[Value]) -> Value {
        score_case(
            "fixture",
            reply,
            "eu-north",
            &["us-east"],
            &["event-7".into()],
            transitions,
            12,
            completions,
        )
    }

    #[test]
    fn correct_answer_with_fabricated_extra_citation_fails() {
        let result = evaluate(
            r#"{"answer":"eu-north","sourceIds":["event-7","invented"]}"#,
            Transitions::default(),
            &[],
        );
        assert_eq!(result["valid"], false);
    }

    #[test]
    fn missing_required_transitions_fail_despite_correct_answer() {
        for transitions in [
            Transitions {
                requires_compaction: true,
                ..Default::default()
            },
            Transitions {
                requires_restart: true,
                ..Default::default()
            },
        ] {
            assert_eq!(
                evaluate(
                    r#"{"answer":"eu-north","sourceIds":["event-7"]}"#,
                    transitions,
                    &[]
                )["valid"],
                false
            );
        }
    }

    #[test]
    fn missing_usage_does_not_become_a_partial_total() {
        let result = evaluate(
            r#"{"answer":"eu-north","sourceIds":["event-7"]}"#,
            Transitions::default(),
            &[
                json!({"usage":{"input_tokens":100,"output_tokens":5,"provider_metadata":{"cost":0.01}}}),
                json!({"usage":{"input_tokens":20}}),
            ],
        );
        assert_eq!(result["usage"]["inputTokens"], 120);
        assert!(result["usage"]["outputTokens"].is_null());
        assert!(result["usage"]["cost"].is_null());
    }

    #[test]
    fn stale_or_malformed_answer_fails() {
        for reply in [
            "eu-north",
            r#"{"answer":"us-east","sourceIds":["event-7"]}"#,
            r#"{"answer":"not eu-north","sourceIds":["event-7"]}"#,
        ] {
            assert_eq!(evaluate(reply, Transitions::default(), &[])["valid"], false);
        }
    }

    #[test]
    fn correct_answer_and_observed_transitions_pass() {
        let result = evaluate(
            r#"{"answer":"EU-NORTH","sourceIds":["event-7"]}"#,
            Transitions {
                requires_compaction: true,
                compacted: true,
                requires_restart: true,
                restarted: true,
            },
            &[],
        );
        assert_eq!(result["valid"], true);
        assert_eq!(result["accuracy"], 1.0);
        assert_eq!(result["latencyMs"], 12);
    }
}
