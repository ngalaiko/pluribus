#![allow(unsafe_op_in_unsafe_fn)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;

use pluribus_plugin_sdk::export;
pub use pluribus_plugin_sdk::{exports, http, pluribus, wasi};

use crate::http::Reader;
use crate::http::{Header, InlineRequest, Request};
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::blobs;
use pluribus::plugin::credentials;
use pluribus::plugin::events;
use pluribus::plugin::types::{self, Error, ErrorCode, Event, Payload, Proposal};

use pluribus_model::{
    BlobRef, COMPLETION_SCHEMA, Completion, ContentPart, Delta, Feature, MediaPart, Message,
    MessageRole, Request as ModelRequest, STREAM_SCHEMA, StopReason, Stream, ToolCall,
    ToolCallDelta, Usage,
};

// Configuration arrives in `init`; no import returns it later.
thread_local! {
    static CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
}

const URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEVICE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const ACCOUNT_CLAIM: &str = "https://api.openai.com/auth";
const DEFAULT_MODEL: &str = "gpt-5.6-luna";
const DEFAULT_TIMEOUT_MS: u32 = 300_000;
const CHUNK_BYTES: u32 = 1024 * 1024;
const MAX_PROVIDER_TOOL_NAME_BYTES: usize = 64;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(rename = "subscription")]
    subscription: String,
}

/// The enrolled record behind the subscription handle. The plugin owns it:
/// the host attaches nothing and refreshes nothing.
#[derive(Deserialize, Serialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct DeviceState {
    id: String,
    user_code: String,
    interval_seconds: u64,
    expires_at_ms: i64,
    #[serde(default)]
    next_poll_at_ms: i64,
    #[serde(default)]
    authorization_code: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    enrollment_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    credentials: Credentials,
    #[serde(default = "default_models")]
    models: Vec<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u32,
}

struct Codex;

use pluribus_plugin_sdk::serve;

fn setup(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
    let parsed: Config = serde_json::from_slice(&config)
        .map_err(|error| invalid_argument(format!("invalid configuration: {error}")))?;
    if parsed.credentials.subscription.is_empty() {
        return Err(invalid_argument("credential handle is empty"));
    }
    if parsed.models.is_empty() {
        return Err(invalid_argument("at least one model is required"));
    }
    let handle = parsed.credentials.subscription.clone();
    CONFIG.with_borrow_mut(|slot| *slot = Some(parsed));
    let mut outcome = empty_outcome();
    if let Ok((previous, mut record)) = credential_record(&handle)
        && let Some(mut device) = record
            .get("device")
            .and_then(|value| serde_json::from_value::<DeviceState>(value.clone()).ok())
    {
        let due = if device.next_poll_at_ms != 0 {
            device.next_poll_at_ms
        } else {
            now_ms().saturating_add(interval_ms(device.interval_seconds))
        };
        if device.next_poll_at_ms != due {
            device.next_poll_at_ms = due;
            record["device"] = serde_json::to_value(&device).map_err(internal)?;
            if !save_credential_record(&handle, previous.as_deref(), &record)? {
                return Ok(outcome);
            }
        }
        outcome.events.push(enrollment_timer(&device, due, None));
    }
    Ok(outcome)
}

impl Guest for Codex {
    async fn run(context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        serve::<Self>(context).await
    }

    async fn handle(context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let config = config()?;
        let mut proposals = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            checkpoint = Some(event.sequence);
            if event.event_type == "credential.enrollment.requested" {
                let request = event_value(event)?;
                if request["component"] != context.instance_id
                    || request["credential"] != config.credentials.subscription
                    || event.actor.kind != types::PrincipalKind::Node
                    || event.actor.id != "credential-cli"
                {
                    continue;
                }
                let enrollment = request["enrollment"]
                    .as_str()
                    .ok_or_else(|| invalid("enrollment request has no id"))?;
                let emitted = start_device_enrollment(
                    &config.credentials.subscription,
                    enrollment,
                    event.event_id.as_str(),
                )?;
                proposals.extend(emitted);
                continue;
            }
            if event.event_type == "timer.fired" {
                if !matches!(
                    event.actor.kind,
                    types::PrincipalKind::Component | types::PrincipalKind::Node
                ) {
                    continue;
                }
                let mut timer = event_value(event)?;
                let Some(request_id) = timer["requestEventId"].as_str() else {
                    continue;
                };
                let Ok(request) = events::get(request_id) else {
                    continue;
                };
                if request.event_type != "timer.set"
                    || request.actor.kind != types::PrincipalKind::Component
                    || request.actor.id != context.instance_id
                {
                    continue;
                }
                let request_value = event_value(&request)?;
                if request_value["dueAtMs"] != timer["dueAtMs"] {
                    continue;
                }
                let Some(enrollment_id) = request_value["enrollmentId"].as_str() else {
                    continue;
                };
                timer["enrollmentId"] = Value::String(enrollment_id.to_owned());
                let emitted = poll_device_enrollment(
                    &config.credentials.subscription,
                    &timer,
                    event.event_id.as_str(),
                )?;
                proposals.extend(emitted);
                continue;
            }
            if event.event_type != "model.requested" {
                continue;
            }
            let request: ModelRequest = match decode_request(event) {
                Ok(request) => request,
                Err(error) => {
                    proposals.push(failure_event(event, None, &error)?);
                    continue;
                }
            };
            if !config.models.iter().any(|model| model == &request.model) {
                continue;
            }
            match complete(&request, &config) {
                Ok(completion) => proposals.push(proposal(
                    "model.completed",
                    COMPLETION_SCHEMA,
                    &serde_json::to_value(&completion).map_err(internal)?,
                    None,
                    Some(event.event_id.clone()),
                )?),
                Err(error) => {
                    proposals.push(failure_event(event, Some(&request.call_id), &error)?);
                }
            }
        }

        Ok(Outcome {
            events: proposals,
            mutations: Vec::new(),
            checkpoint,
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(empty_outcome())
    }
}

/// Runs one completion against the Codex Responses endpoint.
fn complete(request: &ModelRequest, config: &Config) -> Result<Completion, Error> {
    reject_unsupported_features(&request.required_features)?;
    let (body, tool_names) = build_request(request, config.reasoning_effort.as_deref())?;
    let body = put_blob("application/json", &body)?;
    let handle = &config.credentials.subscription;
    let (record, tokens) = read_tokens(handle)?;
    let read = responses(&body, &tokens, config.timeout_ms)?;
    // An expired access token is rejected before any frame arrives, so one
    // refresh replays the request.
    let mut read = if read.status() == 401 {
        drop(read);
        let tokens = refresh(handle, record, &tokens)?;
        responses(&body, &tokens, config.timeout_ms)?
    } else {
        read
    };
    // The reader closes when it drops, ending the transfer.
    parse_stream(&request.call_id, &mut read, tool_names)
}

/// Opens one Responses stream under the supplied tokens.
fn responses(body: &types::BlobRef, tokens: &Tokens, timeout_ms: u32) -> Result<Reader, Error> {
    http::sse(&Request {
        method: "POST".to_owned(),
        url: URL.to_owned(),
        headers: vec![
            header("accept", "text/event-stream"),
            header("content-type", "application/json"),
            header("OpenAI-Beta", "responses=experimental"),
            header("originator", "pluribus"),
            header("authorization", &format!("Bearer {}", tokens.access_token)),
            header("chatgpt-account-id", &account_id(&tokens.access_token)?),
        ],
        body: Some(body.clone()),
        timeout_ms,
    })
}

/// Reads the sealed record along with the bytes a swap must match.
fn read_tokens(handle: &str) -> Result<(Vec<u8>, Tokens), Error> {
    let bytes =
        credentials::get(handle)?.ok_or_else(|| invalid("Codex subscription is not enrolled"))?;
    let tokens =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid credential record"))?;
    Ok((bytes, tokens))
}

/// Exchanges the refresh token and seals the replacement under the handle.
/// A lost swap adopts whatever the winner stored.
fn refresh(handle: &str, previous: Vec<u8>, tokens: &Tokens) -> Result<Tokens, Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("refresh_token", &tokens.refresh_token)
        .finish();
    let response = http::exchange(&InlineRequest {
        method: "POST".to_owned(),
        url: TOKEN_URL.to_owned(),
        headers: vec![header("content-type", "application/x-www-form-urlencoded")],
        body: body.into_bytes(),
        timeout_ms: 30_000,
    })?;
    if !(200..300).contains(&response.status) {
        return Err(unavailable(format!(
            "Codex token refresh returned HTTP {}",
            response.status
        )));
    }
    let value = parse_json(&response.body)?;
    let refreshed = Tokens {
        access_token: required_string(&value, "access_token")?.to_owned(),
        refresh_token: option_string(&value, "refresh_token")
            .unwrap_or(&tokens.refresh_token)
            .to_owned(),
    };
    let mut record = parse_json(&previous)?;
    if !record.is_object() {
        return Err(invalid("invalid credential record"));
    }
    record["access_token"] = Value::String(refreshed.access_token.clone());
    record["refresh_token"] = Value::String(refreshed.refresh_token.clone());
    let bytes = serde_json::to_vec(&record).map_err(internal)?;
    if credentials::compare_and_swap(handle, Some(&previous), &bytes)? {
        return Ok(refreshed);
    }
    Ok(read_tokens(handle)?.1)
}

fn event_value(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => parse_json(bytes),
        Payload::Blob(_) => Err(invalid("event payload must be inline JSON")),
    }
}

fn now_ms() -> i64 {
    let time = wasi::clocks::system_clock::now();
    time.seconds
        .saturating_mul(1_000)
        .saturating_add(i64::from(time.nanoseconds / 1_000_000))
}

fn credential_record(handle: &str) -> Result<(Option<Vec<u8>>, Value), Error> {
    let previous = credentials::get(handle)?;
    let record = previous
        .as_deref()
        .map(parse_json)
        .transpose()?
        .unwrap_or_else(|| json!({}));
    if !record.is_object() {
        return Err(invalid("invalid credential record"));
    }
    Ok((previous, record))
}

fn save_credential_record(
    handle: &str,
    previous: Option<&[u8]>,
    record: &Value,
) -> Result<bool, Error> {
    let bytes = serde_json::to_vec(record).map_err(internal)?;
    credentials::compare_and_swap(handle, previous, &bytes)
}

fn seal_tokens(
    handle: &str,
    previous: Option<&[u8]>,
    record: Value,
    tokens: Tokens,
) -> Result<bool, Error> {
    let mut expected = previous.map(ToOwned::to_owned);
    let mut candidate = record.clone();
    for _ in 0..4 {
        if let Some(enrollment_id) = candidate["device"]["enrollment_id"].as_str() {
            candidate["completed_enrollment"] = Value::String(enrollment_id.to_owned());
        }
        candidate["device"] = Value::Null;
        candidate["enrollment"] = Value::Null;
        candidate["access_token"] = Value::String(tokens.access_token.clone());
        candidate["refresh_token"] = Value::String(tokens.refresh_token.clone());
        if let Some(expected) = expected.as_deref()
            && credentials::compare_and_swap(
                handle,
                Some(expected),
                &serde_json::to_vec(&candidate).map_err(internal)?,
            )?
        {
            return Ok(true);
        }
        let Some(latest_bytes) = credentials::get(handle)? else {
            return Err(unavailable("credential disappeared during enrollment"));
        };
        let latest = parse_json(&latest_bytes)?;
        if !pending_record_matches(&latest, &record) {
            return Ok(false);
        }
        expected = Some(latest_bytes);
        candidate = latest;
    }
    Err(unavailable("credential changed during enrollment"))
}

fn pending_record_matches(actual: &Value, expected: &Value) -> bool {
    let Some(actual_device) = actual.get("device").and_then(Value::as_object) else {
        return false;
    };
    let Some(expected_device) = expected.get("device").and_then(Value::as_object) else {
        return false;
    };
    for field in [
        "id",
        "user_code",
        "enrollment_id",
        "authorization_code",
        "code_verifier",
    ] {
        if actual_device.get(field) != expected_device.get(field) {
            return false;
        }
    }
    actual.get("enrollment").and_then(|value| value.get("id"))
        == expected.get("enrollment").and_then(|value| value.get("id"))
}

fn enrollment_started(device: &DeviceState, cause: &str) -> Proposal {
    Proposal {
        event_type: "credential.enrollment.started".into(),
        payload_schema: "pluribus.credential.enrollment.started/1".into(),
        payload: Payload::Json(
            serde_json::to_vec(&json!({
                "url": DEVICE_VERIFICATION_URL,
                "userCode": device.user_code,
            }))
            .unwrap(),
        ),
        idempotency_key: Some(format!("codex:enrollment:{}:started", device.enrollment_id)),
        causation_id: Some(cause.to_owned()),
    }
}

fn enrollment_timer(device: &DeviceState, due_at_ms: i64, cause: Option<&str>) -> Proposal {
    Proposal {
        event_type: "timer.set".into(),
        payload_schema: "pluribus.timer-set/1".into(),
        payload: Payload::Json(
            serde_json::to_vec(&json!({
                "dueAtMs": due_at_ms,
                "enrollmentId": device.enrollment_id,
            }))
            .unwrap(),
        ),
        idempotency_key: Some(format!(
            "codex:enrollment:{}:timer:{}",
            device.enrollment_id, due_at_ms
        )),
        causation_id: cause.map(str::to_owned),
    }
}

fn start_device_enrollment(
    handle: &str,
    enrollment_id: &str,
    cause: &str,
) -> Result<Vec<Proposal>, Error> {
    let (previous, mut record) = credential_record(handle)?;
    if record["completed_enrollment"].as_str() == Some(enrollment_id) {
        return Ok(Vec::new());
    }
    let staged_id = record
        .get("enrollment")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str);
    if staged_id.is_none()
        && let Some(device) = record
            .get("device")
            .and_then(|value| serde_json::from_value::<DeviceState>(value.clone()).ok())
            .filter(|device| device.enrollment_id == enrollment_id)
    {
        let due = if device.next_poll_at_ms != 0 {
            device.next_poll_at_ms
        } else {
            now_ms().saturating_add(interval_ms(device.interval_seconds))
        };
        return Ok(vec![
            enrollment_started(&device, cause),
            enrollment_timer(&device, due, Some(cause)),
        ]);
    }
    let Some(staged) = record.get("enrollment") else {
        return Ok(Vec::new());
    };
    if staged["id"].as_str() != Some(enrollment_id) {
        return Ok(Vec::new());
    }
    let expires_at_ms = staged["expires_at_ms"]
        .as_i64()
        .ok_or_else(|| invalid("credential enrollment has no expiry"))?;
    if expires_at_ms <= now_ms() {
        record["enrollment"] = Value::Null;
        record["completed_enrollment"] = Value::String(enrollment_id.to_owned());
        if !save_credential_record(handle, previous.as_deref(), &record)? {
            return Err(unavailable("credential changed during enrollment"));
        }
        return Ok(Vec::new());
    }
    let response = http::exchange(&InlineRequest {
        method: "POST".into(),
        url: DEVICE_URL.into(),
        headers: vec![header("content-type", "application/json")],
        body: serde_json::to_vec(&json!({"client_id": CLIENT_ID})).map_err(internal)?,
        timeout_ms: 30_000,
    })?;
    if !(200..300).contains(&response.status) {
        return Err(unavailable(format!(
            "Codex device enrollment returned HTTP {}",
            response.status
        )));
    }
    let value = parse_json(&response.body)?;
    let mut device = DeviceState {
        id: required_string(&value, "device_auth_id")?.to_owned(),
        user_code: required_string(&value, "user_code")?.to_owned(),
        interval_seconds: integer_field(&value, "interval").unwrap_or(5).max(1) as u64,
        expires_at_ms: now_ms().saturating_add(
            integer_field(&value, "expires_in")
                .unwrap_or(900)
                .clamp(1, 900)
                .saturating_mul(1_000),
        ),
        next_poll_at_ms: 0,
        authorization_code: None,
        code_verifier: None,
        enrollment_id: enrollment_id.to_owned(),
    };
    device.next_poll_at_ms = now_ms().saturating_add(interval_ms(device.interval_seconds));
    record["device"] = serde_json::to_value(&device).map_err(internal)?;
    record["enrollment"] = Value::Null;
    if !save_credential_record(handle, previous.as_deref(), &record)? {
        return Err(unavailable("credential changed during enrollment"));
    }
    let due = device.next_poll_at_ms;
    Ok(vec![
        enrollment_started(&device, cause),
        enrollment_timer(&device, due, Some(cause)),
    ])
}

fn poll_device_enrollment(
    handle: &str,
    timer: &Value,
    cause: &str,
) -> Result<Vec<Proposal>, Error> {
    let (previous, mut record) = credential_record(handle)?;
    let Some(device) = record
        .get("device")
        .and_then(|value| serde_json::from_value::<DeviceState>(value.clone()).ok())
    else {
        return Ok(Vec::new());
    };
    if timer["enrollmentId"].as_str() != Some(device.enrollment_id.as_str()) {
        return Ok(Vec::new());
    }
    if record
        .get("enrollment")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .is_some_and(|id| id != device.enrollment_id)
    {
        return Ok(Vec::new());
    }
    let Some(fired_due_at_ms) = timer.get("dueAtMs").and_then(Value::as_i64) else {
        return Ok(Vec::new());
    };
    if device.next_poll_at_ms != 0 && fired_due_at_ms != device.next_poll_at_ms {
        return if fired_due_at_ms < device.next_poll_at_ms {
            Ok(vec![enrollment_timer(
                &device,
                device.next_poll_at_ms,
                Some(cause),
            )])
        } else {
            Ok(Vec::new())
        };
    }
    let now = now_ms();
    if fired_due_at_ms > now {
        return Ok(Vec::new());
    }
    if device.expires_at_ms <= now {
        record["completed_enrollment"] = Value::String(device.enrollment_id.clone());
        record["device"] = Value::Null;
        if !save_credential_record(handle, previous.as_deref(), &record)? {
            return Err(unavailable("credential changed during enrollment"));
        }
        return Ok(Vec::new());
    }
    if let (Some(code), Some(verifier)) = (
        device.authorization_code.as_deref(),
        device.code_verifier.as_deref(),
    ) {
        let tokens = exchange_device_code(code, verifier)?;
        if !seal_tokens(handle, previous.as_deref(), record, tokens)? {
            return Ok(Vec::new());
        }
        return Ok(Vec::new());
    }
    let body = serde_json::to_vec(&json!({
        "device_auth_id": device.id,
        "user_code": device.user_code,
    }))
    .map_err(internal)?;
    let response = http::exchange(&InlineRequest {
        method: "POST".into(),
        url: DEVICE_TOKEN_URL.into(),
        headers: vec![header("content-type", "application/json")],
        body,
        timeout_ms: 30_000,
    })?;
    if !(200..300).contains(&response.status) {
        let error = response_error(&response.body);
        let terminal = matches!(
            error.as_deref(),
            Some("slow_down") | Some("access_denied") | Some("expired_token")
        );
        if error.as_deref() == Some("deviceauth_authorization_pending")
            || ((response.status == 403 || response.status == 404) && !terminal)
        {
            let mut next = device.clone();
            next.next_poll_at_ms = now.saturating_add(interval_ms(next.interval_seconds));
            record["device"] = serde_json::to_value(&next).map_err(internal)?;
            if !save_credential_record(handle, previous.as_deref(), &record)? {
                return Err(unavailable("credential changed during enrollment"));
            }
            return Ok(vec![enrollment_timer(
                &next,
                next.next_poll_at_ms,
                Some(cause),
            )]);
        }
        if error.as_deref() == Some("slow_down") {
            let mut next = device.clone();
            next.interval_seconds = next.interval_seconds.saturating_add(5);
            next.next_poll_at_ms = now.saturating_add(interval_ms(next.interval_seconds));
            record["device"] = serde_json::to_value(&next).map_err(internal)?;
            if !save_credential_record(handle, previous.as_deref(), &record)? {
                return Err(unavailable("credential changed during enrollment"));
            }
            return Ok(vec![enrollment_timer(
                &next,
                next.next_poll_at_ms,
                Some(cause),
            )]);
        }
        if response.status == 429 || response.status >= 500 {
            let mut next = device.clone();
            next.next_poll_at_ms = now.saturating_add(interval_ms(next.interval_seconds));
            record["device"] = serde_json::to_value(&next).map_err(internal)?;
            if !save_credential_record(handle, previous.as_deref(), &record)? {
                return Err(unavailable("credential changed during enrollment"));
            }
            return Ok(vec![enrollment_timer(
                &next,
                next.next_poll_at_ms,
                Some(cause),
            )]);
        }
        record["completed_enrollment"] = Value::String(device.enrollment_id.clone());
        record["device"] = Value::Null;
        if !save_credential_record(handle, previous.as_deref(), &record)? {
            return Err(unavailable("credential changed during enrollment"));
        }
        return Ok(Vec::new());
    }
    let value = parse_json(&response.body)?;
    let code = required_string(&value, "authorization_code")?;
    let verifier = required_string(&value, "code_verifier")?;
    let mut pending = device.clone();
    pending.authorization_code = Some(code.to_owned());
    pending.code_verifier = Some(verifier.to_owned());
    record["device"] = serde_json::to_value(&pending).map_err(internal)?;
    let pending_bytes = serde_json::to_vec(&record).map_err(internal)?;
    if !credentials::compare_and_swap(handle, previous.as_deref(), &pending_bytes)? {
        return Err(unavailable("credential changed during enrollment"));
    }
    let tokens = exchange_device_code(code, verifier)?;
    if !seal_tokens(handle, Some(&pending_bytes), record, tokens)? {
        return Ok(Vec::new());
    }
    Ok(Vec::new())
}

fn response_error(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.as_str().or_else(|| error.get("code")?.as_str()))
        .map(str::to_owned)
}

fn integer_field(value: &Value, field: &str) -> Option<i64> {
    value
        .get(field)
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn interval_ms(seconds: u64) -> i64 {
    seconds.saturating_mul(1_000).min(i64::MAX as u64) as i64
}

fn exchange_device_code(code: &str, verifier: &str) -> Result<Tokens, Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code", code)
        .append_pair("code_verifier", verifier)
        .append_pair(
            "redirect_uri",
            "https://auth.openai.com/deviceauth/callback",
        )
        .finish();
    let response = http::exchange(&InlineRequest {
        method: "POST".into(),
        url: TOKEN_URL.into(),
        headers: vec![header("content-type", "application/x-www-form-urlencoded")],
        body: body.into_bytes(),
        timeout_ms: 30_000,
    })?;
    if !(200..300).contains(&response.status) {
        return Err(unavailable(format!(
            "Codex device token exchange returned HTTP {}",
            response.status
        )));
    }
    let value = parse_json(&response.body)?;
    Ok(Tokens {
        access_token: required_string(&value, "access_token")?.to_owned(),
        refresh_token: required_string(&value, "refresh_token")?.to_owned(),
    })
}

/// The account the subscription belongs to, from the access token's claims.
fn account_id(access_token: &str) -> Result<String, Error> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or_else(|| invalid("access token is not a JWT"))?;
    let claims = parse_json(&base64url(payload)?)?;
    claims[ACCOUNT_CLAIM]["chatgpt_account_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("access token names no ChatGPT account"))
}

fn base64url(input: &str) -> Result<Vec<u8>, Error> {
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return Err(invalid("access token is not base64url")),
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
        }
    }
    Ok(output)
}

fn decode_request(event: &Event) -> Result<ModelRequest, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| invalid_argument(format!("invalid model request: {error}"))),
        Payload::Blob(_) => Err(invalid_argument("model requests must be inline JSON")),
    }
}

fn failure_event(event: &Event, call_id: Option<&str>, error: &Error) -> Result<Proposal, Error> {
    proposal(
        "model.failed",
        "pluribus.model-failure/1",
        &json!({
            "requestEventId": event.event_id,
            "callId": call_id,
            "code": code_name(error.code),
            "reason": error.message,
        }),
        None,
        Some(event.event_id.clone()),
    )
}

/// Appends one batch of deltas so a completion is visible while it streams.
///
/// The key is the call and the batch ordinal, so a redelivered request
/// deduplicates instead of doubling the stream.
fn emit_stream(call_id: &str, ordinal: u64, deltas: Vec<Delta>) -> Result<(), Error> {
    if deltas.is_empty() {
        return Ok(());
    }
    let stream = Stream {
        call_id: call_id.to_owned(),
        deltas,
    };
    let proposal = proposal(
        "model.stream",
        STREAM_SCHEMA,
        &serde_json::to_value(&stream).map_err(internal)?,
        Some(format!("model:{call_id}:stream:{ordinal}")),
        None,
    )?;
    events::append(&proposal).map(|_| ())
}

fn proposal(
    event_type: &str,
    payload_schema: &str,
    value: &Value,
    idempotency_key: Option<String>,
    causation_id: Option<String>,
) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: event_type.to_owned(),
        payload_schema: payload_schema.to_owned(),
        payload: Payload::Json(serde_json::to_vec(value).map_err(internal)?),
        idempotency_key,
        causation_id,
    })
}

const fn empty_outcome() -> Outcome {
    Outcome {
        events: Vec::new(),
        mutations: Vec::new(),
        checkpoint: None,
    }
}

const fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidArgument => "invalid-argument",
        ErrorCode::NotFound => "not-found",
        ErrorCode::PermissionDenied => "permission-denied",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::ResourceExhausted => "resource-exhausted",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::DeadlineExceeded => "deadline-exceeded",
        ErrorCode::Internal => "internal",
    }
}

fn config() -> Result<Config, Error> {
    CONFIG.with_borrow(|slot| {
        slot.clone()
            .ok_or_else(|| unavailable("plugin is not initialized"))
    })
}

fn build_request(
    request: &ModelRequest,
    reasoning_effort: Option<&str>,
) -> Result<(Vec<u8>, ToolNames), Error> {
    let options = request
        .provider_options
        .clone()
        .unwrap_or_else(|| json!({}));
    let instructions = system_instructions(&request.messages);
    let tool_names = ToolNames::new(&request.tools);
    let mut input = continuation_items(request.continuation.as_ref())?;
    input.extend(convert_messages(&request.messages, &tool_names)?);
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            Ok(json!({
                "type": "function",
                "name": tool_names.provider(&tool.name),
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": false,
            }))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("store".into(), Value::Bool(false)),
        ("stream".into(), Value::Bool(true)),
        (
            "instructions".into(),
            Value::String(if instructions.is_empty() {
                "You are a helpful assistant.".into()
            } else {
                instructions
            }),
        ),
        ("input".into(), Value::Array(input)),
        ("parallel_tool_calls".into(), Value::Bool(true)),
        ("tool_choice".into(), Value::String("auto".into())),
        ("include".into(), json!(["reasoning.encrypted_content"])),
        (
            "text".into(),
            json!({"verbosity": option_string(&options, "text_verbosity").unwrap_or("low")}),
        ),
    ]);
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    // Codex rejects per-request output-token limits.
    if let Some(key) = option_string(&options, "prompt_cache_key") {
        body.insert("prompt_cache_key".into(), Value::String(key.to_owned()));
    }
    if let Some(effort) = option_string(&options, "reasoning_effort").or(reasoning_effort) {
        body.insert(
            "reasoning".into(),
            json!({
                "effort": effort,
                "summary": option_string(&options, "reasoning_summary").unwrap_or("auto")
            }),
        );
    }
    if let Some(schema) = &request.output_schema {
        body.insert(
            "text".into(),
            json!({
                "verbosity": option_string(&options, "text_verbosity").unwrap_or("low"),
                "format": {
                    "type": "json_schema",
                    "name": "output",
                    "strict": true,
                    "schema": schema,
                }
            }),
        );
    }
    let body = serde_json::to_vec(&body)
        .map_err(|error| internal(format!("cannot encode request: {error}")))?;
    Ok((body, tool_names))
}

fn system_instructions(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|message| matches!(message.role, MessageRole::System))
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn convert_messages(messages: &[Message], tool_names: &ToolNames) -> Result<Vec<Value>, Error> {
    let mut result = Vec::new();
    for message in messages {
        if matches!(message.role, MessageRole::System) {
            continue;
        }
        let role = match message.role {
            MessageRole::System => unreachable!(),
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let mut content = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text { text } => content.push(json!({
                    "type": if role == "assistant" { "output_text" } else { "input_text" },
                    "text": text,
                })),
                ContentPart::ToolCall(call) => result.push(json!({
                    "type": "function_call",
                    "call_id": call.call_id,
                    "name": tool_names.provider(&call.name),
                    "arguments": serde_json::to_string(&call.arguments)
                        .map_err(internal)?,
                })),
                ContentPart::ToolResult(tool_result) => {
                    result.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_result.call_id,
                        "output": serde_json::to_string(&tool_result.output).map_err(internal)?,
                    }));
                }
                ContentPart::ProviderData { data } => result.push(data.clone()),
                ContentPart::Image(media) => content.push(image_part(media)?),
                ContentPart::Audio(_) => {
                    return Err(unsupported("audio input is not implemented"));
                }
            }
        }
        if !content.is_empty() {
            result.push(json!({"role": role, "content": content}));
        }
    }
    Ok(result)
}

fn image_part(media: &MediaPart) -> Result<Value, Error> {
    let bytes = read_blob(&media.blob)?;
    let encoded = base64(&bytes);
    Ok(json!({
        "type": "input_image",
        "image_url": format!("data:{};base64,{encoded}", media.blob.media_type),
        "detail": media.detail.as_deref().unwrap_or("auto"),
    }))
}

fn continuation_items(continuation: Option<&BlobRef>) -> Result<Vec<Value>, Error> {
    let Some(blob) = continuation else {
        return Ok(Vec::new());
    };
    let value = parse_json(&read_blob(blob)?)?;
    value
        .get("reasoning")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid("continuation has no reasoning array"))
}

#[derive(Default)]
struct StreamState {
    text: String,
    reasoning_summary: String,
    tools: Vec<ToolCall>,
    terminal: Option<Value>,
    tool_names: ToolNames,
    /// Deltas awaiting their next `model.stream` batch. Batching keeps a long
    /// completion from writing one durable event per token.
    pending: Vec<Delta>,
    batch: u64,
}

/// Reads the response stream and accumulates one completion.
///
/// The host hands over raw bytes rather than parsed frames, so SSE record
/// framing lives here: the transport moves bytes, the plugin owns the
/// protocol. The core enforces cancellation through epoch interruption, so
/// there is no cancellation flag to poll.
fn parse_stream(
    call_id: &str,
    read: &mut Reader,
    tool_names: ToolNames,
) -> Result<Completion, Error> {
    let mut state = StreamState {
        tool_names,
        ..StreamState::default()
    };
    let mut buffered = Vec::new();
    loop {
        let chunk = read.receive(CHUNK_BYTES, 1_000)?;
        buffered.extend_from_slice(&chunk.bytes);
        let (records, rest) = split_sse(&buffered)?;
        buffered = rest;
        for record in records {
            if record == b"[DONE]" {
                continue;
            }
            let event = parse_json(&record)?;
            if process_event(&event, &mut state)? {
                return finish_response(call_id, state);
            }
        }
        // One batch per read, so progress is observable without a durable
        // event per token.
        flush(call_id, &mut state)?;
        if chunk.closed {
            return Err(unavailable("stream ended without a terminal event"));
        }
    }
}

/// Splits complete SSE records out of the buffer, returning the unconsumed
/// tail so a record spanning two reads is not truncated.
fn split_sse(buffered: &[u8]) -> Result<(Vec<Vec<u8>>, Vec<u8>), Error> {
    let text = std::str::from_utf8(buffered).map_err(|_| invalid("SSE response is not UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    let Some(boundary) = normalized.rfind("\n\n") else {
        return Ok((Vec::new(), buffered.to_vec()));
    };
    let (complete, tail) = normalized.split_at(boundary + 2);
    Ok((sse_data(complete.as_bytes())?, tail.as_bytes().to_vec()))
}

fn process_event(event: &Value, state: &mut StreamState) -> Result<bool, Error> {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "response.output_text.delta" => {
            let delta = required_string(event, "delta")?;
            state.text.push_str(delta);
            state.pending.push(Delta::text(delta));
        }
        "response.reasoning_summary_text.delta" => {
            let delta = required_string(event, "delta")?;
            state.reasoning_summary.push_str(delta);
            state.pending.push(Delta::reasoning_summary(delta));
        }
        "response.function_call_arguments.delta" => {
            let index = event
                .get("output_index")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default();
            let delta = required_string(event, "delta")?;
            state.pending.push(Delta::ToolCall(ToolCallDelta {
                index,
                call_id: event
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                name: event
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| state.tool_names.original(name).to_owned()),
                arguments_fragment: delta.to_owned(),
            }));
        }
        "response.output_item.done" => {
            if let Some(call) = event
                .get("item")
                .and_then(|item| parse_tool_call(item, &state.tool_names))
            {
                state.tools.push(call);
            }
        }
        "response.completed" | "response.incomplete" | "response.done" => {
            state.terminal = event.get("response").cloned();
            return Ok(true);
        }
        "response.failed" | "error" => return Err(event_error(event)),
        _ => {}
    }
    Ok(false)
}

fn finish_response(call_id: &str, mut state: StreamState) -> Result<Completion, Error> {
    let response = state
        .terminal
        .take()
        .ok_or_else(|| unavailable("stream ended without a terminal event"))?;
    if state.text.is_empty() {
        state.text = response_text(&response);
    }
    if state.tools.is_empty() {
        state.tools = response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| parse_tool_call(item, &state.tool_names))
            .collect();
    }
    let usage = response.get("usage").map(parse_usage);
    if let Some(usage) = &usage {
        state.pending.push(Delta::Usage(usage.clone()));
    }
    flush(call_id, &mut state)?;
    let mut content = Vec::new();
    if !state.text.is_empty() {
        content.push(ContentPart::text(std::mem::take(&mut state.text)));
    }
    content.extend(state.tools.iter().cloned().map(ContentPart::ToolCall));
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let stop_reason = if !state.tools.is_empty() {
        StopReason::ToolCall
    } else if status == "incomplete" {
        StopReason::MaxOutput
    } else {
        StopReason::EndTurn
    };
    let continuation = reasoning_continuation(&response)?;
    let metadata = json!({
        "id": response.get("id"),
        "status": status,
        "reasoningSummary": state.reasoning_summary,
    });
    Ok(Completion {
        call_id: call_id.to_owned(),
        message: Message {
            role: MessageRole::Assistant,
            name: None,
            content,
        },
        stop_reason,
        usage,
        continuation,
        provider_metadata: Some(metadata),
    })
}

/// Appends whatever deltas have accumulated as one `model.stream` event.
fn flush(call_id: &str, state: &mut StreamState) -> Result<(), Error> {
    if state.pending.is_empty() {
        return Ok(());
    }
    let deltas = std::mem::take(&mut state.pending);
    state.batch += 1;
    emit_stream(call_id, state.batch, deltas)
}

/// Extracts the `data:` payloads from complete SSE records.
fn sse_data(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("SSE response is not UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    let mut records = normalized.split("\n\n").collect::<Vec<_>>();
    if normalized.ends_with("\n\n") {
        records.pop();
    }
    Ok(records
        .into_iter()
        .filter_map(|record| {
            let lines = record
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>();
            (!lines.is_empty()).then(|| lines.join("\n").into_bytes())
        })
        .collect())
}

fn response_text(response: &Value) -> String {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn parse_tool_call(item: &Value, tool_names: &ToolNames) -> Option<ToolCall> {
    if item.get("type")?.as_str()? != "function_call" {
        return None;
    }
    Some(ToolCall {
        call_id: item.get("call_id")?.as_str()?.to_owned(),
        name: tool_names.original(item.get("name")?.as_str()?).to_owned(),
        arguments: serde_json::from_str(
            item.get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}"),
        )
        .unwrap_or_else(|_| json!({})),
    })
}

#[derive(Default)]
struct ToolNames {
    provider_by_original: BTreeMap<String, String>,
    original_by_provider: BTreeMap<String, String>,
}

impl ToolNames {
    fn new(tools: &[pluribus_model::ToolDefinition]) -> Self {
        let mut names = Self::default();
        for (index, tool) in tools.iter().enumerate() {
            let provider = provider_tool_name(&tool.name, index);
            names
                .provider_by_original
                .insert(tool.name.clone(), provider.clone());
            names
                .original_by_provider
                .insert(provider, tool.name.clone());
        }
        names
    }

    fn provider<'a>(&'a self, original: &'a str) -> &'a str {
        self.provider_by_original
            .get(original)
            .map_or(original, String::as_str)
    }

    fn original<'a>(&'a self, provider: &'a str) -> &'a str {
        self.original_by_provider
            .get(provider)
            .map_or(provider, String::as_str)
    }
}

fn provider_tool_name(name: &str, index: usize) -> String {
    let suffix = format!("_t{index}");
    let maximum = MAX_PROVIDER_TOOL_NAME_BYTES.saturating_sub(suffix.len());
    let mut result = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(maximum)
        .collect::<String>();
    if result.is_empty() {
        result.push_str("tool");
    }
    result.push_str(&suffix);
    result
}

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        cached_input_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        provider_metadata: Some(usage.clone()),
    }
}

fn reasoning_continuation(response: &Value) -> Result<Option<BlobRef>, Error> {
    let reasoning = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        .cloned()
        .collect::<Vec<_>>();
    if reasoning.is_empty() {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(&json!({"reasoning": reasoning}))
        .map_err(|error| internal(format!("cannot encode continuation: {error}")))?;
    put_blob("application/json", &bytes).map(|blob| Some(to_model_blob(&blob)))
}

/// The transport and the model payload use different `blob-ref` types, so the
/// boundary converts rather than aliasing them.
fn to_model_blob(blob: &types::BlobRef) -> BlobRef {
    BlobRef {
        algorithm: blob.algorithm.clone(),
        digest: blob.digest.clone(),
        size: blob.size,
        media_type: blob.media_type.clone(),
    }
}

fn to_wit_blob(blob: &BlobRef) -> types::BlobRef {
    types::BlobRef {
        algorithm: blob.algorithm.clone(),
        digest: blob.digest.clone(),
        size: blob.size,
        media_type: blob.media_type.clone(),
    }
}

fn put_blob(media_type: &str, bytes: &[u8]) -> Result<types::BlobRef, Error> {
    let expected =
        u64::try_from(bytes.len()).map_err(|_| resource_exhausted("blob is too large"))?;
    let upload = blobs::open_write(media_type, Some(expected))?;
    // The host reaps an abandoned upload when the delivery ends, so a failed
    // write needs no explicit abort.
    blobs::write(&upload, 0, bytes)?;
    blobs::finish(&upload)
}

fn read_blob(blob: &BlobRef) -> Result<Vec<u8>, Error> {
    read_wit_blob(&to_wit_blob(blob))
}

fn read_wit_blob(blob: &types::BlobRef) -> Result<Vec<u8>, Error> {
    let capacity =
        usize::try_from(blob.size).map_err(|_| resource_exhausted("blob is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    loop {
        let chunk = blobs::read(blob, offset, CHUNK_BYTES)?;
        offset = offset
            .checked_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| resource_exhausted("blob size overflow"))?;
        bytes.extend_from_slice(&chunk.bytes);
        if chunk.closed {
            break;
        }
        if chunk.bytes.is_empty() {
            return Err(internal("blob read made no progress"));
        }
    }
    Ok(bytes)
}

fn reject_unsupported_features(features: &[Feature]) -> Result<(), Error> {
    for feature in features {
        if matches!(feature, Feature::AudioInput | Feature::AudioOutput) {
            return Err(unsupported("requested model feature is unavailable"));
        }
    }
    Ok(())
}

fn parse_json(bytes: &[u8]) -> Result<Value, Error> {
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("invalid JSON: {error}")))
}

fn option_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("event has no {key}")))
}

fn event_error(event: &Value) -> Error {
    let message = event
        .pointer("/response/error/message")
        .or_else(|| event.pointer("/error/message"))
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Codex response failed");
    unavailable(message)
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.to_owned(),
        value: value.as_bytes().to_vec(),
    }
}

fn default_models() -> Vec<String> {
    vec![DEFAULT_MODEL.to_owned()]
}

const fn default_timeout() -> u32 {
    DEFAULT_TIMEOUT_MS
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(char::from(TABLE[usize::from(a >> 2)]));
        output.push(char::from(TABLE[usize::from(((a & 0x03) << 4) | (b >> 4))]));
        output.push(if chunk.len() > 1 {
            char::from(TABLE[usize::from(((b & 0x0f) << 2) | (c >> 6))])
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            char::from(TABLE[usize::from(c & 0x3f)])
        } else {
            '='
        });
    }
    output
}

fn invalid_argument(message: impl Into<String>) -> Error {
    invalid(message)
}

fn invalid(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::InvalidArgument, message, false)
}

fn unsupported(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::Unsupported, message, false)
}

fn unavailable(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::Unavailable, message, true)
}

fn resource_exhausted(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::ResourceExhausted, message, false)
}

fn internal(message: impl std::fmt::Display) -> Error {
    plugin_error(ErrorCode::Internal, message.to_string(), false)
}

fn plugin_error(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Error {
    Error {
        code,
        message: message.into(),
        retryable,
        details: None,
    }
}

export!(Codex);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_tool_schema_and_name_round_trip() {
        let schema = json!({
            "type":"object",
            "properties":{"action":{"type":"string","enum":["complete","wait"]}},
            "required":["action"],
            "additionalProperties":false
        });
        let request: ModelRequest = serde_json::from_value(json!({
            "call_id":"test", "model":"gpt-5.6-luna", "messages":[],
            "tools":[
                {"name":"js","description":"Compute","input_schema":{"type":"object"}},
                {"name":"yield","description":"Return control","input_schema":schema}
            ]
        }))
        .unwrap();
        let (bytes, names) = build_request(&request, None).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["tools"][1]["parameters"], schema);
        let arguments = json!({"action":"complete"});
        let call = parse_tool_call(
            &json!({
                "type":"function_call", "call_id":"control",
                "name":body["tools"][1]["name"],
                "arguments":arguments.to_string()
            }),
            &names,
        )
        .unwrap();
        assert_eq!(call.name, "yield");
        assert_eq!(call.arguments, arguments);
    }

    #[test]
    fn codex_omits_unsupported_output_token_parameter() {
        let request: ModelRequest = serde_json::from_value(json!({
            "call_id":"test", "model":"gpt-5.6-luna", "messages":[],
            "max_output_tokens":4096
        }))
        .unwrap();
        let (bytes, _) = build_request(&request, None).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn configured_reasoning_effort_is_a_request_default() {
        for (configured, options, expected) in [
            (None, json!({}), None),
            (Some("medium"), json!({}), Some("medium")),
            (None, json!({"reasoning_effort":"high"}), Some("high")),
            (
                Some("medium"),
                json!({"reasoning_effort":"high", "reasoning_summary":"detailed"}),
                Some("high"),
            ),
        ] {
            let mut config = json!({"credentials":{"subscription":"test"}});
            if let Some(effort) = configured {
                config["reasoning_effort"] = json!(effort);
            }
            let config: Config = serde_json::from_value(config).unwrap();
            let request: ModelRequest = serde_json::from_value(json!({
                "call_id":"test", "model":"gpt-5.6-luna", "messages":[],
                "provider_options":options
            }))
            .unwrap();
            let (bytes, _) = build_request(&request, config.reasoning_effort.as_deref()).unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            if let Some(effort) = expected {
                assert_eq!(body["reasoning"]["effort"], effort);
                assert_eq!(
                    body["reasoning"]["summary"],
                    options
                        .get("reasoning_summary")
                        .cloned()
                        .unwrap_or(json!("auto"))
                );
            } else {
                assert!(body.get("reasoning").is_none());
            }
        }
    }

    #[test]
    fn parses_unterminated_terminal_sse_record() {
        let records = sse_data(
            br#"data: {"type":"response.output_text.delta","delta":"hello"}

data: {"type":"response.completed","response":{"status":"completed"}}"#,
        )
        .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(
            parse_json(&records[1]).unwrap()["type"],
            "response.completed"
        );
    }

    #[test]
    fn encodes_base64_padding() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
