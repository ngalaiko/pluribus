use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Timing {
    At {
        at: String,
    },
    After {
        milliseconds: i64,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}

impl Timing {
    pub fn first(&self, now: i64) -> Result<i64, String> {
        let due = match self {
            Self::At { at } => DateTime::parse_from_rfc3339(at)
                .map_err(|e| e.to_string())?
                .timestamp_millis(),
            Self::After { milliseconds } if *milliseconds > 0 => now
                .checked_add(*milliseconds)
                .ok_or("delay exceeds timestamp range")?,
            Self::After { .. } => return Err("milliseconds must be positive".into()),
            Self::Cron { .. } => {
                return self.next(now)?.ok_or("cron has no next occurrence".into());
            }
        };
        if due <= now {
            return Err("scheduled time must be in the future".into());
        }
        Ok(due)
    }

    pub fn next(&self, now: i64) -> Result<Option<i64>, String> {
        let Self::Cron {
            expression,
            timezone,
        } = self
        else {
            return Ok(None);
        };
        if expression.split_whitespace().count() != 5 {
            return Err("cron requires five fields: minute hour day month weekday".into());
        }
        let timezone = Tz::from_str(timezone).map_err(|e| e.to_string())?;
        let cron = Cron::from_str(expression).map_err(|e| e.to_string())?;
        let now = DateTime::<Utc>::from_timestamp_millis(now)
            .ok_or("timestamp out of range")?
            .with_timezone(&timezone);
        cron.find_next_occurrence(&now, false)
            .map(|time| Some(time.timestamp_millis()))
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub prompt: String,
    pub timing: Timing,
    pub next_at_ms: Option<i64>,
    pub last_at_ms: Option<i64>,
    pub paused: bool,
    pub error: Option<String>,
    pub origin_event_id: String,
    pub origin: Value,
    pub cause: String,
}

impl Schedule {
    pub fn occurrence(&mut self, now: i64) -> Option<(String, Value)> {
        let due = self.next_at_ms.filter(|due| *due <= now && !self.paused)?;
        let key = format!("schedule:{}:{}:{due}", self.id, self.revision);
        let mut observation = self.origin.clone();
        observation["originEventId"] = self.origin_event_id.clone().into();
        observation["message"] = serde_json::json!({"text": self.prompt});
        observation["schedule"] = serde_json::json!({
            "id": self.id, "name": self.name, "revision": self.revision,
            "scheduledAtMs": due, "emittedAtMs": now,
        });
        self.last_at_ms = Some(due);
        // Coalesce missed occurrences; recurrence remains aligned to the calendar.
        match self.timing.next(now) {
            Ok(next) => self.next_at_ms = next,
            Err(error) => {
                self.next_at_ms = None;
                self.error = Some(error);
            }
        }
        Some((key, observation))
    }

    pub fn owned_by(&self, origin: &Value) -> bool {
        ["provider", "externalSenderId", "conversationId"]
            .iter()
            .all(|field| self.origin[field] == origin[field])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ms(time: &str) -> i64 {
        DateTime::parse_from_rfc3339(time)
            .unwrap()
            .timestamp_millis()
    }

    fn schedule(timing: Timing, now: i64) -> Schedule {
        Schedule {
            id: "one".into(),
            revision: 1,
            name: "check".into(),
            prompt: "Check the build".into(),
            next_at_ms: Some(timing.first(now).unwrap()),
            timing,
            last_at_ms: None,
            paused: false,
            error: None,
            origin_event_id: "observation".into(),
            origin: json!({"provider":"cli", "externalSenderId":"user", "conversationId":"chat"}),
            cause: "request".into(),
        }
    }

    #[test]
    fn absolute_and_relative_one_shots_fire_once_after_restore() {
        for timing in [
            Timing::At {
                at: "2026-09-23T12:00:01Z".into(),
            },
            Timing::After { milliseconds: 1000 },
        ] {
            let now = ms("2026-09-23T12:00:00Z");
            let mut schedule = schedule(timing, now);
            assert!(schedule.occurrence(now).is_none());
            let (_, observation) = schedule.occurrence(now + 1000).unwrap();
            assert_eq!(observation["message"]["text"], "Check the build");
            assert_eq!(observation["originEventId"], "observation");
            let mut restored: Schedule =
                serde_json::from_slice(&serde_json::to_vec(&schedule).unwrap()).unwrap();
            assert!(restored.occurrence(now + 2000).is_none());
        }
    }

    #[test]
    fn cron_coalesces_downtime_and_preserves_wall_clock_time() {
        let mut schedule = schedule(
            Timing::Cron {
                expression: "0 9 * * MON-FRI".into(),
                timezone: "Europe/Stockholm".into(),
            },
            ms("2026-09-23T00:00:00Z"),
        );
        assert_eq!(schedule.next_at_ms, Some(ms("2026-09-23T07:00:00Z")));
        assert!(schedule.occurrence(ms("2026-09-28T10:00:00Z")).is_some());
        assert_eq!(schedule.next_at_ms, Some(ms("2026-09-29T07:00:00Z")));
        assert!(schedule.occurrence(ms("2026-09-28T10:00:00Z")).is_none());
    }

    #[test]
    fn cron_changes_utc_offset_across_dst() {
        let timing = Timing::Cron {
            expression: "0 9 * * *".into(),
            timezone: "Europe/Stockholm".into(),
        };
        assert_eq!(
            timing.first(ms("2026-03-28T09:00:00Z")).unwrap(),
            ms("2026-03-29T07:00:00Z")
        );
        assert_eq!(
            timing.first(ms("2026-10-24T09:00:00Z")).unwrap(),
            ms("2026-10-25T08:00:00Z")
        );
    }

    #[test]
    fn invalid_rules_are_rejected() {
        for timing in [
            Timing::After { milliseconds: 0 },
            Timing::After { milliseconds: -1 },
            Timing::After {
                milliseconds: i64::MAX,
            },
            Timing::At {
                at: "2020-01-01T00:00:00".into(),
            },
            Timing::Cron {
                expression: "* * * * * *".into(),
                timezone: "UTC".into(),
            },
            Timing::Cron {
                expression: "0 9 * * *".into(),
                timezone: "Mars".into(),
            },
            Timing::Cron {
                expression: "0 0 31 2 *".into(),
                timezone: "UTC".into(),
            },
        ] {
            assert!(timing.first(1000).is_err(), "{timing:?}");
        }
    }

    #[test]
    fn paused_schedules_do_not_fire_and_ownership_is_conversation_scoped() {
        let mut schedule = schedule(Timing::After { milliseconds: 1 }, 0);
        schedule.paused = true;
        assert!(schedule.occurrence(1000).is_none());
        assert!(!schedule.owned_by(
            &json!({"provider":"cli", "externalSenderId":"other", "conversationId":"chat"})
        ));
        assert!(!schedule.owned_by(
            &json!({"provider":"cli", "externalSenderId":"user", "conversationId":"other"})
        ));
    }
}
