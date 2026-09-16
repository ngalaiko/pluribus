#![allow(unsafe_op_in_unsafe_fn)]
wit_bindgen::generate!({ generate_all,path:"../../wit",world:"plugin"});
mod auth;
#[allow(dead_code)]
mod common;
use base64::{Engine, engine::general_purpose::STANDARD};
use common::*;
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::{
    events, state,
    types::{Error, Event},
};
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(rename = "app")]
    app: String,
}

#[derive(Clone, Deserialize)]
struct Config {
    credentials: Credentials,
    http_instance: String,
    route_id: String,
    owner: String,
}
thread_local! {static CONFIG:std::cell::RefCell<Option<Config>>=const{std::cell::RefCell::new(None)};}
struct Github;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../shared/run.rs"));

fn setup(_: Context, config: Vec<u8>) -> Result<Outcome, Error> {
    let config: Config =
        serde_json::from_slice(&config).map_err(|_| error("invalid GitHub config"))?;
    if config.owner.is_empty()
        || !config
            .owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(error("invalid GitHub configuration"));
    }
    CONFIG.with_borrow_mut(|c| *c = Some(config));
    let mut out = empty(None);
    let due = now_ms();
    out.events.push(timer(due, None));
    out.mutations.push(refresh_state(due));
    Ok(out)
}

impl Guest for Github {
    async fn run(context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        serve::<Self>(context).await
    }

    async fn handle(context: Context, input: Vec<Event>) -> Result<Outcome, Error> {
        let config = CONFIG
            .with_borrow(|c| c.clone())
            .ok_or_else(|| error("GitHub not initialized"))?;
        let mut out = empty(input.last().map(|e| e.sequence));
        let mut staged = std::collections::BTreeMap::new();
        let mut active = state::get("refresh/due")?
            .map(|bytes| serde_json::from_slice::<i64>(&bytes))
            .transpose()
            .map_err(|_| error("invalid refresh state"))?;
        for event in &input {
            let now = now_ms();
            if event.event_type == "timer.fired" {
                if !refresh_due(active, value(event)?["dueAtMs"].as_i64()) {
                    continue;
                }
                let _ = auth::refresh(&config, now);
                active = Some(now + 60000);
                out.mutations.push(refresh_state(now + 60000));
                out.events
                    .push(timer(now + 60000, Some(event.event_id.clone())));
                continue;
            }
            if event.event_type == "credential.enrollment.requested" {
                let request = value(event)?;
                if event.actor.kind != pluribus::plugin::types::PrincipalKind::Node
                    || event.actor.id != "credential-cli"
                    || request["component"] != context.instance_id
                    || request["credential"] != config.credentials.app
                {
                    continue;
                }
                let result =
                    auth::enroll(&config, now, request["enrollment"].as_str().unwrap_or(""));
                let payload = match result {
                    Ok(url) => json!({"url":url}),
                    Err(_) => json!({"error":"enrollment unavailable"}),
                };
                out.events.push(proposal(
                    "credential.enrollment.started",
                    payload,
                    Some(event.event_id.clone()),
                ));
                continue;
            }
            if event.event_type != "http.request.received" || event.actor.id != config.http_instance
            {
                continue;
            }
            let request = value(event)?;
            if request["routeId"] != config.route_id || request["consumer"] != context.instance_id {
                continue;
            }
            if request["target"].as_str().unwrap_or("").split('?').next() != Some("/events") {
                let mut reply = auth::response(404, "Not found");
                reply["requestEventId"] = json!(event.event_id);
                out.events.push(proposal(
                    "http.response.requested",
                    reply,
                    Some(event.event_id.clone()),
                ));
                continue;
            }
            let (mut status, mut observation) = normalize(&config, &request, &context.instance_id)?;
            if let Some(p) = &observation {
                let key = p.idempotency_key.clone().unwrap();
                if let Some(body) = staged.get(&key) {
                    status = if body == &request["body"] { 200 } else { 409 };
                    observation = None;
                } else {
                    staged.insert(key, request["body"].clone());
                }
            }
            if let Some(mut p) = observation {
                p.causation_id = Some(event.event_id.clone());
                out.events.push(p);
            }
            out.events.push(proposal(
                "http.response.requested",
                json!({"requestEventId":event.event_id,"status":status,"body":""}),
                Some(event.event_id.clone()),
            ));
        }
        Ok(out)
    }
    fn stop(_: Context, _: i64) -> Result<Outcome, Error> {
        Ok(empty(None))
    }
}
fn empty(checkpoint: Option<u64>) -> Outcome {
    Outcome {
        events: vec![],
        mutations: vec![],
        checkpoint,
    }
}
fn header(request: &Value, name: &str) -> Option<String> {
    let matches: Vec<_> = request["headers"]
        .as_array()?
        .iter()
        .filter(|pair| {
            pair[0]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(name))
        })
        .collect();
    if matches.len() != 1 {
        return None;
    }
    String::from_utf8(STANDARD.decode(matches[0][1].as_str()?).ok()?).ok()
}
fn normalize(
    config: &Config,
    request: &Value,
    instance: &str,
) -> Result<(u16, Option<pluribus::plugin::types::Proposal>), Error> {
    if request["method"] != "POST" {
        return Ok((405, None));
    }
    let Some(sig) = header(request, "x-hub-signature-256") else {
        return Ok((401, None));
    };
    let Some(kind) = header(request, "x-github-event").filter(|s| {
        !s.is_empty()
            && s.len() < 128
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }) else {
        return Ok((400, None));
    };
    let Some(id) = header(request, "x-github-delivery").filter(|s| {
        !s.is_empty() && s.len() < 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) else {
        return Ok((400, None));
    };
    let (previous, mut doc) = match auth::load(config) {
        Ok(v) => v,
        Err(_) => return Ok((503, None)),
    };
    let Some(secret) = doc["app"]["webhook_secret"].as_str() else {
        return Ok((503, None));
    };
    let Ok(bytes) = STANDARD.decode(request["body"].as_str().unwrap_or("")) else {
        return Ok((400, None));
    };
    if !auth::verify(secret, &sig, &bytes) {
        return Ok((401, None));
    }
    let verified = json!({"appId":doc["app"]["id"],"installationId":doc["installation_id"]});
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok((400, None));
    };
    if !admitted(&body, &kind, &config.owner, &verified) {
        return Ok((403, None));
    }
    if kind.starts_with("installation") {
        if matches!(body["action"].as_str(), Some("deleted" | "suspend")) {
            doc["installation_id"] = Value::Null;
            doc["exports"] = json!({});
        } else {
            doc["installation_id"] = body["installation"]["id"].clone();
        }
        auth::save(config, previous.as_deref(), &doc)?;
    }
    // Committed observations are the durable deduplication source, including after state rebuild.
    let mut after = None;
    loop {
        let page = events::query(
            &events::Filter {
                after_sequence: after,
                event_types: vec!["observation.received".into()],
                correlation_id: None,
                activity_id: None,
                recorded_from_ms: None,
                recorded_to_ms: None,
            },
            1,
        )?;
        if page.events.is_empty() {
            break;
        }
        for previous in &page.events {
            if previous.actor.id == instance && previous.event_type == "observation.received" {
                let old = value(previous)?;
                if old["provider"] == "github"
                    && old["routeId"] == config.route_id
                    && old["deliveryId"] == id
                {
                    return Ok((
                        if old["rawBody"] == request["body"] {
                            200
                        } else {
                            409
                        },
                        None,
                    ));
                }
            }
        }
        let next = page.next_sequence;
        if next.is_none() || next == after {
            break;
        }
        after = next;
    }
    let repo = body["repository"]["full_name"]
        .as_str()
        .unwrap_or(&config.owner);
    let mut p = proposal(
        "observation.received",
        json!({"provider":"github","trusted":false,"routeId":config.route_id,"deliveryId":id,"event":kind,"action":body["action"],"repository":body["repository"],"conversationId":format!("github:{repo}"),"externalSenderId":body["sender"]["id"].to_string(),"observedAtMs":request["receivedAtMs"],"message":{"text":format!("GitHub {kind} {} in {repo}",body["action"].as_str().unwrap_or(""))},"payload":body,"rawBody":request["body"]}),
        None,
    );
    p.idempotency_key = Some(format!("github:{}:{id}", config.route_id));
    Ok((200, Some(p)))
}
fn admitted(body: &Value, kind: &str, owner: &str, verified: &Value) -> bool {
    let owns = |v: &Value| {
        v["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(owner))
            && v["type"] == "User"
    };
    if kind == "ping" {
        return body["hook"].is_object() && body["hook_id"].as_u64().is_some();
    }
    if kind.starts_with("installation") {
        return owns(&body["installation"]["account"])
            && body["installation"]["app_id"] == verified["appId"];
    }
    owns(&body["repository"]["owner"])
        && verified["installationId"].as_u64().is_some()
        && body["installation"]["id"] == verified["installationId"]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_headers_are_rejected() {
        let r = json!({"headers":[["x-github-event",STANDARD.encode("push")],["X-GitHub-Event",STANDARD.encode("ping")]]});
        assert!(header(&r, "x-github-event").is_none());
    }
    #[test]
    fn wrong_owner_and_installation_are_rejected() {
        let v = json!({"appId":7,"installationId":9});
        let mut b =
            json!({"repository":{"owner":{"login":"me","type":"User"}},"installation":{"id":9}});
        assert!(admitted(&b, "push", "me", &v));
        b["installation"]["id"] = json!(10);
        assert!(!admitted(&b, "push", "me", &v));
        assert!(!admitted(&b, "push", "other", &v));
    }
}
export!(Github);

fn refresh_due(active: Option<i64>, fired: Option<i64>) -> bool {
    active.is_some() && active == fired
}
#[cfg(test)]
mod refresh_tests {
    #[test]
    fn stale_refresh_timers_do_not_start_another_chain() {
        assert!(!super::refresh_due(Some(200), Some(100)));
        assert!(!super::refresh_due(None, Some(100)));
        assert!(super::refresh_due(Some(200), Some(200)));
    }
}

fn refresh_state(due: i64) -> pluribus::plugin::types::Mutation {
    pluribus::plugin::types::Mutation::Set(pluribus::plugin::types::StateEntry {
        key: "refresh/due".into(),
        value: serde_json::to_vec(&due).unwrap(),
    })
}

fn now_ms() -> i64 {
    let t = wasi::clocks::system_clock::now();
    t.seconds * 1000 + i64::from(t.nanoseconds / 1_000_000)
}

#[path = "../../shared/http.rs"]
pub mod http;
