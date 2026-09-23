#![allow(unsafe_op_in_unsafe_fn)]

//! A mail connector: one IMAP session observing arrivals, and SMTP
//! submission for what the agent answers with.
//!
//! The host owns both connections: it resolves each granted endpoint, checks
//! the certificate, speaks the STARTTLS preamble where submission needs one,
//! and enforces the byte and connection ceilings. This component owns IMAP,
//! SMTP, MIME, and the cursor that makes redelivery idempotent.
//!
//! Sending happens inside `handle`, on its own connection to its own
//! endpoint. The IMAP session is suspended for the duration and resumes
//! where it was: no command is interleaved, and the cursor is untouched.

mod imap;
mod mime;
mod observation;
mod send;
mod smtp;
mod wire;

use pluribus_plugin_sdk::export;
pub use pluribus_plugin_sdk::{exports, pluribus, wasi};

use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::types::{Error, ErrorCode, Event};
use pluribus::plugin::{credentials, runtime};
use serde::Deserialize;
use serde_json::Value;
use wire::{Connection, failure};

use pluribus_plugin_sdk::socket as channel;

/// How long one IDLE wait blocks before the loop looks around. Short enough
/// that a stop is noticed promptly, and that the renewal clock advances.
const IDLE_TICK_MS: u32 = 30_000;
/// IMAP allows 29 minutes between IDLE renewals. Renewing at 25 leaves room
/// for a slow round trip. RFC 2177.
const IDLE_RENEW_MS: u64 = 25 * 60 * 1000;
/// Reconnect backoff bounds.
const BACKOFF_MIN_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 300_000;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialSlots {
    account: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    credentials: CredentialSlots,
    #[serde(default = "default_mailbox")]
    mailbox: String,
    /// Largest message fetched whole. A larger one is observed by its header,
    /// marked truncated.
    #[serde(default = "default_max_message_bytes")]
    max_message_bytes: u64,
    /// Largest attachment stored as a blob.
    #[serde(default = "default_max_attachment_bytes")]
    max_attachment_bytes: u64,
    /// Messages fetched before committing and looking around.
    #[serde(default = "default_batch")]
    batch: usize,
    /// Observe what is already in the mailbox on first connection. Off by
    /// default: a first run records the boundary and observes arrivals.
    #[serde(default)]
    import_history: bool,
    /// The address outgoing mail is sent from. The credential's username
    /// stands in when this is absent, which is the account's own address
    /// wherever submission authenticates with it.
    #[serde(default)]
    from: Option<String>,
    /// The display name outgoing mail carries beside the address.
    #[serde(default)]
    display_name: Option<String>,
}

fn default_mailbox() -> String {
    "INBOX".into()
}
const fn default_max_message_bytes() -> u64 {
    2 * 1024 * 1024
}
const fn default_max_attachment_bytes() -> u64 {
    8 * 1024 * 1024
}
const fn default_batch() -> usize {
    20
}

/// The enrolled credential, as `pluribus plugins auth` stored it. One account
/// authenticates both endpoints.
#[derive(Deserialize)]
struct Account {
    username: String,
    password: String,
}

thread_local! {
    static CONFIG: std::cell::RefCell<Option<Config>> = const { std::cell::RefCell::new(None) };
}

fn config() -> Result<Config, Error> {
    CONFIG.with_borrow(|config| {
        config
            .clone()
            .ok_or_else(|| failure(ErrorCode::Internal, "component is not configured"))
    })
}

struct Email;

impl Guest for Email {
    async fn run(mut context: Context, config: Vec<u8>) -> Result<(), Error> {
        setup(config)?;
        runtime::ready(vec![], vec![]).await?;
        let mut backoff = 0;
        loop {
            let result = Self::waiting(&mut context, session()).await;
            // A stop unwinds the session; whatever it returned on the way out
            // is not a transport failure to retry.
            if stopping() {
                return Ok(());
            }
            match result {
                Ok(None) => return Ok(()),
                Ok(Some(())) => backoff = BACKOFF_MIN_MS,
                // A rejected credential is rejected again on every attempt,
                // and hammering an endpoint with one locks accounts.
                Err(error) if !error.retryable => return Err(error),
                Err(_) => {
                    backoff = backoff
                        .saturating_mul(2)
                        .clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS);
                }
            }
            // The wait also renews the call budget the next connect is
            // measured against.
            if Self::waiting(&mut context, sleep(backoff)).await?.is_none() {
                return Ok(());
            }
        }
    }

    async fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let mut proposals = Vec::new();
        let mut checkpoint = None;
        for event in &events {
            checkpoint = Some(event.sequence);
            if event.event_type != "capability.requested" {
                continue;
            }
            let request = send::json_payload(event)?;
            let Some(capability) = request.get("capability").and_then(Value::as_str) else {
                continue;
            };
            if !send::CAPABILITIES.contains(&capability) {
                continue;
            }
            proposals.push(send::dispatch(event, capability, &request, &config()?, now_ms()).await);
        }
        Ok(Outcome {
            events: proposals,
            mutations: vec![],
            checkpoint,
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: None,
        })
    }
}

fn setup(config: Vec<u8>) -> Result<(), Error> {
    let parsed: Config = serde_json::from_slice(&config).map_err(|error| {
        failure(
            ErrorCode::InvalidArgument,
            format!("invalid config: {error}"),
        )
    })?;
    if parsed.batch == 0 || parsed.mailbox.is_empty() {
        return Err(failure(
            ErrorCode::InvalidArgument,
            "mailbox must be named and batch must be positive",
        ));
    }
    CONFIG.with_borrow_mut(|slot| *slot = Some(parsed));
    Ok(())
}

// Set once a stop has been delivered, so an unwinding session is not
// mistaken for a transport failure worth retrying.
thread_local! {
    static STOPPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn stopping() -> bool {
    STOPPING.with(std::cell::Cell::get)
}

async fn sleep(ms: u64) -> Result<(), Error> {
    wasi::clocks::monotonic_clock::wait_for(ms.saturating_mul(1_000_000)).await;
    Ok(())
}

fn now_ms() -> i64 {
    let now = wasi::clocks::system_clock::now();
    now.seconds * 1000 + i64::from(now.nanoseconds / 1_000_000)
}

fn monotonic_ms() -> u64 {
    wasi::clocks::monotonic_clock::now() / 1_000_000
}

/// One connected session: authenticate, catch up, then idle until the
/// connection ends. Returns when the endpoint goes away.
async fn session() -> Result<(), Error> {
    let config = config()?;
    let account = load_credential(&config)?;
    let mut connection = Connection::new(channel::Socket::connect("imap").await?);
    imap::greet(&mut connection).await?;
    let capabilities = imap::capabilities(&mut connection).await?;
    if !capabilities.iter().any(|name| name == "IDLE") {
        // Polling would be a different connector with a different cost. Say
        // so rather than silently degrading into it.
        return Err(failure(
            ErrorCode::Unsupported,
            "endpoint does not advertise IDLE; this connector observes server push only",
        ));
    }
    imap::login(&mut connection, &account.username, &account.password).await?;
    let mailbox = imap::examine(&mut connection, &config.mailbox).await?;
    let mut cursor =
        observation::resolve_cursor(&config.mailbox, &mailbox, config.import_history, now_ms())?;
    let notice = cursor.resynchronized.take();
    runtime::commit(notice.as_slice(), &[cursor.mutation()?], None)?;
    loop {
        while catch_up(&config, &mut connection, &mut cursor).await? {}
        let tag = imap::idle_begin(&mut connection).await?;
        let entered = monotonic_ms();
        loop {
            if stopping() {
                return Ok(());
            }
            match connection.read(Some(IDLE_TICK_MS)).await? {
                Some(response) if imap::announces_arrival(&response) => break,
                // Any other push is noise to a read-only session; keep waiting.
                Some(_) => {}
                None => {}
            }
            if monotonic_ms().saturating_sub(entered) >= IDLE_RENEW_MS {
                break;
            }
        }
        imap::idle_end(&mut connection, &tag).await?;
    }
}

/// Fetches and commits one batch. `true` when more may be waiting.
async fn catch_up(
    config: &Config,
    connection: &mut Connection,
    cursor: &mut observation::Cursor,
) -> Result<bool, Error> {
    let uids = imap::search_since(connection, cursor.last_uid, config.batch).await?;
    if uids.is_empty() {
        return Ok(false);
    }
    let complete = uids.len() == config.batch;
    let mut proposals = Vec::new();
    for uid in uids {
        if let Some(fetched) = imap::fetch(connection, uid, config.max_message_bytes).await? {
            proposals.push(observation::from_message(
                config,
                cursor,
                &fetched,
                now_ms(),
            )?);
        }
        // An expunged or unreadable UID still advances the cursor: leaving it
        // behind would make every later cycle retry it forever.
        cursor.last_uid = cursor.last_uid.max(uid);
    }
    runtime::commit(&proposals, &[cursor.mutation()?], None)?;
    Ok(complete)
}

fn load_credential(config: &Config) -> Result<Account, Error> {
    let bytes = credentials::get(&config.credentials.account)?.ok_or_else(|| {
        failure(
            ErrorCode::PermissionDenied,
            "no credential is enrolled; run pluribus plugins auth",
        )
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|_| failure(ErrorCode::InvalidArgument, "credential record is malformed"))
}

impl Email {
    /// Services internal deliveries while a session is suspended on I/O.
    async fn waiting<T>(
        context: &mut Context,
        work: impl std::future::Future<Output = Result<T, Error>>,
    ) -> Result<Option<T>, Error> {
        use futures_util::future::{Either, select};
        use runtime::Wake;
        futures_util::pin_mut!(work);
        loop {
            match select(Box::pin(runtime::next()), work.as_mut()).await {
                Either::Left((wake, _)) => match wake? {
                    Wake::Stop(_) => {
                        STOPPING.with(|flag| flag.set(true));
                        // Let the suspended session unwind before its borrowed
                        // streams are dropped.
                        let _ = work.await;
                        return Ok(None);
                    }
                    Wake::Events(events) => match Self::handle(context.clone(), events).await {
                        Ok(outcome) => {
                            runtime::commit(
                                &outcome.events,
                                &outcome.mutations,
                                outcome.checkpoint,
                            )?;
                            context.state_checkpoint =
                                outcome.checkpoint.unwrap_or(context.state_checkpoint);
                        }
                        Err(error) => runtime::reject(&error)?,
                    },
                },
                Either::Right((result, _)) => return result.map(Some),
            }
        }
    }
}

export!(Email);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_requires_a_named_mailbox_and_a_positive_batch() {
        setup(br#"{"credentials":{"account":"mail:account"}}"#.to_vec()).unwrap();
        let config = config().unwrap();
        assert_eq!(config.mailbox, "INBOX");
        assert_eq!(config.batch, 20);
        assert!(!config.import_history);
        assert!(setup(br#"{"credentials":{"account":"a"},"batch":0}"#.to_vec()).is_err());
        assert!(setup(br#"{"credentials":{"account":"a"},"mailbox":""}"#.to_vec()).is_err());
        assert!(setup(br#"{"credentials":{"account":"a"},"unknown":1}"#.to_vec()).is_err());
        assert!(setup(b"{}".to_vec()).is_err());
    }
}
