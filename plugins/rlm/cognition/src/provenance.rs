use crate::compaction::WorkingSummary;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceEvent {
    pub sequence: u64,
    pub original: bool,
}

/// Checks event existence and authorization scope only. It does not establish
/// that an event semantically supports any fact in the summary.
pub(crate) fn validate(
    summary: &WorkingSummary,
    events: &BTreeMap<String, SourceEvent>,
    delegated_range: Option<(u64, u32)>,
) -> Result<(), String> {
    for source_id in &summary.source_ids {
        let Some(event) = events.get(source_id) else {
            return Err("working summary references an inaccessible source event".into());
        };
        if !event.original {
            return Err("working summary source is not an original event".into());
        }
        if delegated_range.is_some_and(|(after, limit)| {
            event.sequence <= after || event.sequence > after.saturating_add(u64::from(limit))
        }) {
            return Err("working summary source is outside the delegated history range".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary() -> WorkingSummary {
        crate::compaction::validate(&json!({
            "version":1,"objective":"x","constraints":[],"decisions":[],
            "completedWork":[],"unresolvedQuestions":[],"durableFacts":[],"corrections":[],
            "sourceIds":["inside"]
        }))
        .unwrap()
    }

    #[test]
    fn existing_event_outside_delegated_range_is_rejected() {
        let mut events = BTreeMap::new();
        events.insert(
            "inside".into(),
            SourceEvent {
                sequence: 9,
                original: true,
            },
        );
        assert!(validate(&summary(), &events, Some((10, 20))).is_err());
    }

    #[test]
    fn inaccessible_and_derived_events_are_rejected() {
        let mut events = BTreeMap::new();
        events.insert(
            "inside".into(),
            SourceEvent {
                sequence: 11,
                original: false,
            },
        );
        assert!(validate(&summary(), &events, None).is_err());
        assert!(validate(&summary(), &BTreeMap::new(), None).is_err());
    }
}
