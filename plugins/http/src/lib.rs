#![allow(unsafe_op_in_unsafe_fn)]
wit_bindgen::generate!({path:"../../wit",world:"source"});
mod common;
use common::*;
use exports::pluribus::plugin::lifecycle::{Context, Guest as Lifecycle, Outcome};
use pluribus::plugin::{
    events, socket, state,
    types::{Error, Event, IngressOutcome, Mutation, StateEntry},
};
use serde_json::json;
struct Http;
impl Lifecycle for Http {
    fn init(_: Context, _: Vec<u8>) -> Result<Outcome, Error> {
        socket::subscribe(b"{\"op\":\"subscribe\"}\n")?;
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: None,
        })
    }
    fn handle(context: Context, input: Vec<Event>) -> Result<Outcome, Error> {
        let output = Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: input.last().map(|e| e.sequence),
        };
        for event in &input {
            if event.event_type == "http.response.requested" {
                let response = value(event)?;
                let Some(id) = response["requestEventId"].as_str() else {
                    continue;
                };
                let Ok(request) = events::get(id) else {
                    continue;
                };
                if request.event_type != "http.request.received"
                    || request.actor.id != context.instance_id
                {
                    continue;
                }
                let original = value(&request)?;
                if original["consumer"].as_str() != Some(event.actor.id.as_str())
                    || event.causation_id.as_deref() != Some(id)
                {
                    continue;
                }
                let mut response = response.clone();
                response["op"] = json!("respond");
                response["requestId"] = original["requestId"].clone();
                // Retrying the event after a transport failure is safe: the listener responds once.
                exchange(response)?;
            }
        }
        Ok(output)
    }
    fn stop(_: Context, _: i64) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: None,
        })
    }
}
impl exports::pluribus::plugin::ingress::Guest for Http {
    fn receive(input: Vec<u8>) -> Result<IngressOutcome, Error> {
        let input: serde_json::Value =
            serde_json::from_slice(&input).map_err(|_| error("invalid stream input"))?;
        let mut output = IngressOutcome {
            events: vec![],
            mutations: vec![],
        };
        if input["kind"] != "socket" {
            return Ok(output);
        }
        let after = state::get("cursor")?
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let session = state::get("listener-session")?.and_then(|b| String::from_utf8(b).ok());
        let result = exchange(json!({"op":"poll","after":after,"session":session}))?;
        let current = result["session"]
            .as_str()
            .ok_or_else(|| error("missing listener session"))?;
        if session.as_deref() != Some(current) {
            output.mutations.push(Mutation::Set(StateEntry {
                key: "listener-session".into(),
                value: current.as_bytes().to_vec(),
            }));
        }
        for message in result["messages"]
            .as_array()
            .ok_or_else(|| error("missing messages"))?
        {
            let mut event = proposal("http.request.received", message["request"].clone(), None);
            event.idempotency_key = Some(format!(
                "http:{}",
                message["request"]["requestId"]
                    .as_str()
                    .ok_or_else(|| error("missing request id"))?
            ));
            output.events.push(event);
            let cursor = message["sequence"]
                .as_u64()
                .ok_or_else(|| error("invalid listener cursor"))?;
            output.mutations.push(Mutation::Set(StateEntry {
                key: "cursor".into(),
                value: cursor.to_string().into_bytes(),
            }));
        }
        Ok(output)
    }
}
export!(Http);
