use crate::schedule::{Schedule, Timing};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub type Change = Option<(String, Option<Schedule>)>;
pub fn manage(
    schedules: &mut BTreeMap<String, Schedule>,
    capability: &str,
    args: &Value,
    request_id: &str,
    origin_id: &str,
    origin: &Value,
    now: i64,
) -> Result<(Value, Change), String> {
    if capability == "schedule.list" {
        let limit = args["limit"].as_u64().unwrap_or(50).clamp(1, 100) as usize;
        let after = args["after"].as_str().unwrap_or("");
        let mut rows = schedules
            .values()
            .filter(|s| s.id.as_str() > after && s.owned_by(origin));
        let page: Vec<_> = rows.by_ref().take(limit).cloned().collect();
        let next = rows.next().and_then(|_| page.last().map(|s| s.id.clone()));
        return Ok((json!({"schedules":page,"nextAfter":next}), None));
    }
    if capability == "schedule.create" {
        let name = text(args, "name", 200)?;
        let prompt = text(args, "prompt", 8192)?;
        let timing: Timing =
            serde_json::from_value(args["timing"].clone()).map_err(|e| e.to_string())?;
        let schedule = Schedule {
            id: request_id.into(),
            revision: 1,
            name,
            prompt,
            next_at_ms: Some(timing.first(now)?),
            timing,
            last_at_ms: None,
            paused: false,
            error: None,
            origin_event_id: origin_id.into(),
            origin: origin.clone(),
            cause: request_id.into(),
        };
        schedules.insert(format!("schedule:{}", schedule.id), schedule.clone());
        return Ok((json!(schedule), Some((schedule.id.clone(), Some(schedule)))));
    }
    let id = text(args, "id", 256)?;
    let key = format!("schedule:{id}");
    let mut schedule = schedules
        .get(&key)
        .filter(|s| s.owned_by(origin))
        .cloned()
        .ok_or("schedule not found in this conversation")?;
    match capability {
        "schedule.get" => return Ok((json!(schedule), None)),
        "schedule.delete" => {
            schedules.remove(&key);
            return Ok((json!({"id":id,"deleted":true}), Some((id, None))));
        }
        "schedule.pause" => schedule.paused = true,
        "schedule.resume" => {
            if schedule.next_at_ms.is_none() {
                return Err("completed or failed schedule requires a new timing rule".into());
            }
            schedule.paused = false;
        }
        "schedule.update" => {
            if args.get("name").is_some() {
                schedule.name = text(args, "name", 200)?;
            }
            if args.get("prompt").is_some() {
                schedule.prompt = text(args, "prompt", 8192)?;
            }
            if let Some(timing) = args.get("timing") {
                let timing: Timing =
                    serde_json::from_value(timing.clone()).map_err(|e| e.to_string())?;
                schedule.next_at_ms = Some(timing.first(now)?);
                schedule.timing = timing;
                schedule.error = None;
            }
        }
        _ => return Err("unknown schedule capability".into()),
    }
    schedule.revision = schedule
        .revision
        .checked_add(1)
        .ok_or("revision exhausted")?;
    schedule.cause = request_id.into();
    schedule.origin_event_id = origin_id.into();
    schedule.origin = origin.clone();
    schedules.insert(key, schedule.clone());
    Ok((json!(schedule), Some((id, Some(schedule)))))
}

pub fn text(value: &Value, field: &str, max: usize) -> Result<String, String> {
    value[field]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= max)
        .map(str::to_owned)
        .ok_or_else(|| format!("{field} must contain 1..{max} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> Value {
        json!({"provider":"cli","externalSenderId":"u","conversationId":"c"})
    }
    fn call(
        schedules: &mut BTreeMap<String, Schedule>,
        method: &str,
        args: Value,
    ) -> Result<(Value, Change), String> {
        manage(
            schedules,
            method,
            &args,
            "request",
            "observation",
            &owner(),
            1000,
        )
    }
    fn create(schedules: &mut BTreeMap<String, Schedule>) {
        call(
            schedules,
            "schedule.create",
            json!({"name":"test","prompt":"do work","timing":{"kind":"after","milliseconds":5000}}),
        )
        .unwrap();
    }
    #[test]
    fn management_covers_create_list_get_update_pause_resume_delete() {
        let mut schedules = BTreeMap::new();
        create(&mut schedules);
        let (list, _) = call(&mut schedules, "schedule.list", json!({})).unwrap();
        assert_eq!(list["schedules"].as_array().unwrap().len(), 1);
        assert_eq!(list["schedules"][0]["nextAtMs"], 6000);
        call(&mut schedules, "schedule.pause", json!({"id":"request"})).unwrap();
        assert!(
            schedules
                .get_mut("schedule:request")
                .unwrap()
                .occurrence(7000)
                .is_none()
        );
        call(&mut schedules,"schedule.update", json!({"id":"request","prompt":"new","timing":{"kind":"cron","expression":"* * * * *","timezone":"UTC"}})).unwrap();
        let (read, _) = call(&mut schedules, "schedule.get", json!({"id":"request"})).unwrap();
        assert_eq!(read["prompt"], "new");
        assert_eq!(read["paused"], true);
        call(&mut schedules, "schedule.resume", json!({"id":"request"})).unwrap();
        assert!(
            schedules
                .get_mut("schedule:request")
                .unwrap()
                .occurrence(60000)
                .is_some()
        );
        call(&mut schedules, "schedule.delete", json!({"id":"request"})).unwrap();
        assert!(schedules.is_empty());
    }
    #[test]
    fn strangers_cannot_read_or_mutate_schedules() {
        let mut schedules = BTreeMap::new();
        create(&mut schedules);
        let foreign = json!({"provider":"cli","externalSenderId":"stranger","conversationId":"c"});
        for method in [
            "schedule.get",
            "schedule.update",
            "schedule.pause",
            "schedule.resume",
            "schedule.delete",
        ] {
            assert!(
                manage(
                    &mut schedules,
                    method,
                    &json!({"id":"request","prompt":"stolen"}),
                    "other",
                    "other-origin",
                    &foreign,
                    1000
                )
                .is_err()
            );
        }
        let (list, _) = manage(
            &mut schedules,
            "schedule.list",
            &json!({}),
            "other",
            "other-origin",
            &foreign,
            1000,
        )
        .unwrap();
        assert!(list["schedules"].as_array().unwrap().is_empty());
        assert_eq!(schedules["schedule:request"].prompt, "do work");
    }
    #[test]
    fn failed_update_preserves_the_schedule() {
        let mut schedules = BTreeMap::new();
        create(&mut schedules);
        assert!(
            call(
                &mut schedules,
                "schedule.update",
                json!({"id":"request","prompt":"new","timing":{"kind":"after","milliseconds":0}})
            )
            .is_err()
        );
        assert_eq!(schedules["schedule:request"].prompt, "do work");
        assert_eq!(schedules["schedule:request"].revision, 1);
    }
}
