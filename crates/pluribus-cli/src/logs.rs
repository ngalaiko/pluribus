use crate::{Paths, SystemMetadata, load_config};
use pluribus_core::{CommittedEvent, EventPayload, EventQuery, EventStore, StreamId};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::{
    error::Error,
    io::{self, Write},
    time::Duration,
};

pub async fn show(paths: &Paths, limit: usize, follow: bool) -> Result<(), Box<dyn Error>> {
    let stream = StreamId::new(load_config(paths)?.agent_id);
    let store = SqliteEventStore::open_read_only(
        paths.state.join("pluribus.sqlite3"),
        SystemMetadata::default(),
    )
    .await?;
    let mut events = store
        .query(
            &stream,
            &EventQuery {
                descending: true,
                ..EventQuery::default()
            },
            limit,
        )
        .await?;
    events.reverse();
    let mut after = 0;
    loop {
        if let Some(last) = events.last() {
            after = last.sequence;
        }
        if let Err(error) = print_events(&events) {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            {
                return Ok(());
            }
            return Err(error);
        }
        if !follow {
            return Ok(());
        }
        if events.len() < 100 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        events = store
            .query(
                &stream,
                &EventQuery {
                    after_sequence: Some(after),
                    ..EventQuery::default()
                },
                100,
            )
            .await?;
    }
}

fn print_events(events: &[CommittedEvent]) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    for event in events {
        let request = &event.request;
        let payload = match &request.payload {
            EventPayload::CanonicalJson(bytes) => serde_json::from_slice::<Value>(bytes)?,
            EventPayload::Blob(blob) => json!({"blob": {
                "algorithm": blob.algorithm, "digest": blob.digest,
                "size": blob.size, "media_type": blob.media_type,
            }}),
        };
        let value = json!({
            "event_id": event.event_id.as_str(),
            "sequence": event.sequence,
            "recorded_at_ms": event.recorded_at_ms,
            "stream_id": request.stream_id.as_str(),
            "event_type": request.event_type,
            "actor": {"kind": format!("{:?}", request.actor.kind).to_lowercase(), "id": request.actor.id.as_str()},
            "payload_schema": request.payload_schema,
            "payload": payload,
            "activity_id": request.activity_id,
            "correlation_id": request.correlation_id,
            "causation_id": request.causation_id.as_ref().map(pluribus_core::EventId::as_str),
        });
        writeln!(output, "{value}")?;
    }
    output.flush()?;
    Ok(())
}
