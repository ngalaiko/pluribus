#![allow(unsafe_op_in_unsafe_fn)]
wit_bindgen::generate!({ generate_all,path:"../../wit",world:"plugin"});
mod common;
use common::*;
use exports::pluribus::plugin::lifecycle::{Context, Guest as Lifecycle, Outcome};
use pluribus::plugin::{
    events, socket, state,
    types::{Error, Event, Mutation, StateEntry},
};
use serde_json::json;
struct Http;
impl Lifecycle for Http {
    async fn run(mut context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        use pluribus::plugin::runtime;
        let mut delay: u32 = 0;
        loop {
            if Self::waiting(&mut context, async {
                crate::wasi::clocks::monotonic_clock::wait_for(u64::from(delay) * 1_000_000).await;
                Ok(())
            })
            .await?
            .is_none()
            {
                return Ok(());
            }

            let result: Result<bool, Error> = async {
                let (read, write) = socket::listen()?;
                let (mut input, completion) = read.read_via_stream().await?;
                let mut completion = Some(completion);
                write.send(b"{\"op\":\"subscribe\"}\n")?;
                loop {
                    let Some(chunk) = Self::waiting(&mut context, async {
                        let (_, bytes) = input.read(Vec::with_capacity(32 * 1024)).await;
                        let closed = bytes.is_empty();
                        if closed {
                            completion.take().unwrap().await?;
                        }
                        Ok(pluribus::plugin::types::Chunk { bytes, closed })
                    })
                    .await?
                    else {
                        return Ok(true);
                    };
                    if !chunk.bytes.is_empty() {
                        let out = Self::poll()?;
                        runtime::commit(&out.events, &out.mutations, None)?;
                    }
                    if chunk.closed {
                        return Err(error("listener disconnected"));
                    }
                }
                #[allow(unreachable_code)]
                Ok(false)
            }
            .await;
            match result {
                Ok(true) => return Ok(()),
                Ok(false) => delay = 100,
                Err(error)
                    if !error.retryable
                        && !matches!(
                            error.code,
                            pluribus::plugin::types::ErrorCode::DeadlineExceeded
                        ) =>
                {
                    return Err(error);
                }
                Err(_) => delay = delay.saturating_mul(2).clamp(100, 30_000),
            }
        }
    }

    async fn handle(context: Context, input: Vec<Event>) -> Result<Outcome, Error> {
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

export!(Http);

impl Http {
    fn poll() -> Result<SourceOutput, Error> {
        let mut output = SourceOutput {
            events: vec![],
            mutations: vec![],
        };
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

struct SourceOutput {
    events: Vec<pluribus::plugin::types::Proposal>,
    mutations: Vec<pluribus::plugin::types::Mutation>,
}

impl Http {
    /// Services internal deliveries while external work is suspended.
    async fn waiting<T>(
        context: &mut Context,
        work: impl std::future::Future<Output = Result<T, Error>>,
    ) -> Result<Option<T>, Error> {
        use futures_util::future::{Either, select};
        use pluribus::plugin::runtime::{self, Wake};
        futures_util::pin_mut!(work);
        loop {
            match select(Box::pin(runtime::next()), work.as_mut()).await {
                Either::Left((wake, _)) => match wake? {
                    Wake::Stop(_) => {
                        // Finish cancelled imports before dropping their borrowed resources.
                        let _ = work.await;
                        return Ok(None);
                    }
                    Wake::Events(events) => match Self::handle(context.clone(), events).await {
                        Ok(out) => {
                            runtime::commit(&out.events, &out.mutations, out.checkpoint)?;
                            context.state_checkpoint =
                                out.checkpoint.unwrap_or(context.state_checkpoint);
                        }
                        Err(error) => runtime::reject(&error)?,
                    },
                },
                Either::Right((result, _)) => return result.map(Some),
            }
        }
    }
}

fn setup(_: Context, _: Vec<u8>) -> Result<Outcome, Error> {
    Ok(Outcome {
        events: vec![],
        mutations: vec![],
        checkpoint: None,
    })
}
