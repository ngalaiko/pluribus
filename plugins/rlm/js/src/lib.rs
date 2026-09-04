#![cfg_attr(not(any(test, target_arch = "wasm32")), allow(dead_code))]
use boa_engine::{Context, Source};
use serde_json::Value;

struct Hooks;
impl boa_engine::context::HostHooks for Hooks {
    fn utc_now(&self) -> i64 {
        0
    }
    fn local_timezone_offset_seconds(&self, _: i64) -> i32 {
        0
    }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // The interpreter exposes deterministic randomness, never cryptographic keys.
    unsafe { std::slice::from_raw_parts_mut(dest, len) }.fill(42);
    Ok(())
}

/// Ceilings on model-authored source. Parsing is neither free nor bounded:
/// the recursive-descent parser recurses once per nesting level, so hostile
/// depth exhausts the stack before any code runs. A trap cannot be reported
/// by the component, an error can be, so the check happens here.
const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_SOURCE_DEPTH: usize = 128;

struct Environment {
    context: Context,
    recovered: bool,
}

/// Rejects source the parser should not see. Brackets inside strings and
/// comments count too, which can refuse legitimate source; the ceiling is
/// far above hand-written nesting.
fn check_source(source: &str) -> Result<(), String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(format!("source exceeds {MAX_SOURCE_BYTES} bytes"));
    }
    let mut depth = 0_usize;
    let mut deepest = 0_usize;
    for byte in source.bytes() {
        match byte {
            b'(' | b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if deepest > MAX_SOURCE_DEPTH {
        Err(format!("source nests deeper than {MAX_SOURCE_DEPTH}"))
    } else {
        Ok(())
    }
}

impl Environment {
    fn new(context: &str) -> Result<Self, String> {
        let mut env = Self {
            recovered: false,
            context: Context::builder()
                .host_hooks(std::rc::Rc::new(Hooks))
                .clock(std::rc::Rc::new(
                    boa_engine::context::time::FixedClock::from_millis(0),
                ))
                .build()
                .map_err(|e| e.to_string())?,
        };
        env.context
            .runtime_limits_mut()
            .set_loop_iteration_limit(1_000_000);
        env.eval(include_str!("bootstrap.js"))?;
        env.refresh_context(context)?;
        Ok(env)
    }

    fn refresh_context(&mut self, context: &str) -> Result<(), String> {
        let mut context: Value = serde_json::from_str(context).map_err(|e| e.to_string())?;
        if self.recovered {
            context
                .as_object_mut()
                .ok_or("context must be an object")?
                .insert("recovered".into(), Value::Bool(true));
        }
        self.eval(&format!("globalThis.context = {context};"))
    }

    fn restore(&mut self, checkpoint: &Value) -> Result<(), String> {
        if checkpoint.is_null() {
            return Ok(());
        }
        if checkpoint["version"] != 1 || !checkpoint["state"].is_object() {
            return Err("unsupported working checkpoint".into());
        }
        let encoded = serde_json::to_string(&checkpoint["state"]).map_err(|e| e.to_string())?;
        if encoded.len() > 32 * 1024 {
            return Err("checkpoint exceeds 32 KiB".into());
        }
        self.eval(&format!(
            "globalThis.state = {encoded}; globalThis.context.recovered = true;"
        ))?;
        self.recovered = true;
        Ok(())
    }

    fn eval(&mut self, code: &str) -> Result<(), String> {
        self.context
            .eval(Source::from_bytes(code))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn step(&mut self, method: &str, input: &str) -> Result<String, String> {
        let input = if method == "__start" {
            check_source(input)?;
            serde_json::to_string(input).map_err(|e| e.to_string())?
        } else {
            serde_json::from_str::<Value>(input)
                .map_err(|e| e.to_string())?
                .to_string()
        };
        self.eval(&format!("{method}({input});"))?;
        self.context.run_jobs().map_err(|e| e.to_string())?;
        let result = self
            .context
            .eval(Source::from_bytes("JSON.stringify(__poll())"))
            .map_err(|e| e.to_string())?;
        let text = result
            .as_string()
            .ok_or("invalid environment result")?
            .to_std_string_escaped();
        if text.len() > 1024 * 1024 {
            return Err("environment output exceeds 1 MiB".into());
        }
        Ok(text)
    }
}

#[cfg(target_arch = "wasm32")]
mod component {
    use super::*;
    use std::cell::RefCell;
    wit_bindgen::generate!({path: "../../../wit", world: "plugin"});
    use exports::pluribus::plugin::lifecycle::{Context as CallContext, Guest, Outcome};
    use pluribus::plugin::types::{Error, ErrorCode, Event, Payload, Proposal};
    use serde_json::json;

    // A running cell is a paused async function, not serializable state. The
    // manifest declares a pinned session so the host keeps this instance for
    // the activity's deliveries; losing it fails the activity.
    thread_local! { static ENV: RefCell<std::collections::BTreeMap<String, Environment>> = const { RefCell::new(std::collections::BTreeMap::new()) }; }

    struct Code;

    impl Guest for Code {
        fn init(_context: CallContext, _config: Vec<u8>) -> Result<Outcome, Error> {
            Ok(empty())
        }

        fn handle(_context: CallContext, events: Vec<Event>) -> Result<Outcome, Error> {
            let mut proposals = Vec::new();
            let mut checkpoint = None;

            for event in &events {
                checkpoint = Some(event.sequence);
                let payload = match &event.payload {
                    Payload::Json(bytes) => serde_json::from_slice::<Value>(bytes)
                        .map_err(|error| failure(format!("invalid payload: {error}")))?,
                    Payload::Blob(_) => continue,
                };
                if event.event_type == "code.close-requested" {
                    ENV.with(|env| {
                        env.borrow_mut().remove(&session_id(event));
                    });
                    proposals.push(proposal(
                        "code.closed",
                        &json!({"sessionId":session_id(event)}),
                        Some(event.event_id.clone()),
                    )?);
                    continue;
                }
                let stepped = match event.event_type.as_str() {
                    "code.evaluate-requested" => start_cell(&payload),
                    "code.resumed" => resume_cell(&payload),
                    _ => continue,
                };
                proposals.push(match stepped {
                    Ok(status) => outcome_event(event, &status)?,
                    Err(message) => proposal(
                        "code.failed",
                        &json!({"requestEventId": event.event_id, "sessionId": session_id(event), "reason": message}),
                        Some(event.event_id.clone()),
                    )?,
                });
            }

            Ok(Outcome {
                events: proposals,
                mutations: Vec::new(),
                checkpoint,
            })
        }

        fn stop(_context: CallContext, _deadline_at_ms: i64) -> Result<Outcome, Error> {
            ENV.with(|env| env.borrow_mut().clear());
            Ok(empty())
        }
    }

    fn start_cell(payload: &Value) -> Result<Value, String> {
        let context = payload
            .get("context")
            .map(ToString::to_string)
            .unwrap_or_else(|| "{}".to_owned());
        let source = payload
            .get("source")
            .and_then(Value::as_str)
            .ok_or("source must be a string")?
            .to_owned();
        let session = payload["sessionId"].as_str().unwrap_or("default");
        ENV.with(|env| -> Result<(), String> {
            let mut sessions = env.borrow_mut();
            if let Some(environment) = sessions.get_mut(session) {
                environment.refresh_context(&context)?;
            } else {
                if payload["requiresSession"] == true && payload["checkpoint"].is_null() {
                    return Err("session was lost; no working checkpoint; replan the cell".into());
                }
                if sessions.len() >= 128 {
                    return Err("session limit exceeded".into());
                }
                let mut environment = Environment::new(&context)?;
                environment.restore(&payload["checkpoint"])?;
                sessions.insert(session.into(), environment);
            }
            Ok(())
        })?;
        step(session, "__start", &source)
    }

    fn resume_cell(payload: &Value) -> Result<Value, String> {
        let response = payload
            .get("response")
            .ok_or("response is required")?
            .to_string();
        step(
            payload["sessionId"].as_str().unwrap_or("default"),
            "__resume",
            &response,
        )
    }

    fn step(session: &str, method: &str, input: &str) -> Result<Value, String> {
        let text = ENV.with(|env| {
            env.borrow_mut()
                .get_mut(session)
                .ok_or("environment not initialized; session was lost")?
                .step(method, input)
        })?;
        serde_json::from_str(&text).map_err(|error| format!("invalid cell status: {error}"))
    }

    /// Turns one `__poll` status into the event that answers the delivery.
    fn outcome_event(event: &Event, status: &Value) -> Result<Proposal, Error> {
        let kind = status.get("status").and_then(Value::as_str).unwrap_or("");
        match kind {
            "request" => proposal(
                "code.yielded",
                &json!({
                    "requestEventId": event.event_id, "sessionId": session_id(event),
                    "id": status.get("id"),
                    "method": status.get("method"),
                    "args": status.get("args"),
                    "log": status.get("log"),
                }),
                Some(event.event_id.clone()),
            ),
            "done" => proposal(
                "code.completed",
                &json!({
                    "requestEventId": event.event_id, "sessionId": session_id(event),
                    "value": status.get("value"),
                    "checkpoint": status.get("checkpoint"),
                    "log": status.get("log"),
                }),
                Some(event.event_id.clone()),
            ),
            "error" => proposal(
                "code.failed",
                &json!({
                    "requestEventId": event.event_id, "sessionId": session_id(event),
                    "reason": status.get("error"),
                    "log": status.get("log"),
                }),
                Some(event.event_id.clone()),
            ),
            other => proposal(
                "code.failed",
                &json!({
                    "requestEventId": event.event_id, "sessionId": session_id(event),
                    "reason": format!("cell suspended without a host request: {other}"),
                }),
                Some(event.event_id.clone()),
            ),
        }
    }

    fn session_id(event: &Event) -> String {
        match &event.payload {
            Payload::Json(bytes) => serde_json::from_slice::<Value>(bytes)
                .ok()
                .and_then(|v| v["sessionId"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "default".into()),
            Payload::Blob(_) => "default".into(),
        }
    }

    fn proposal(
        event_type: &str,
        value: &Value,
        causation_id: Option<String>,
    ) -> Result<Proposal, Error> {
        Ok(Proposal {
            event_type: event_type.to_owned(),
            payload_schema: "dev.pluribus.js.cell/1".to_owned(),
            payload: Payload::Json(
                serde_json::to_vec(value)
                    .map_err(|error| failure(format!("cannot encode payload: {error}")))?,
            ),
            idempotency_key: None,
            causation_id,
        })
    }

    const fn empty() -> Outcome {
        Outcome {
            events: Vec::new(),
            mutations: Vec::new(),
            checkpoint: None,
        }
    }

    fn failure(message: impl Into<String>) -> Error {
        Error {
            code: ErrorCode::Internal,
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    export!(Code);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn value(text: String) -> Value {
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn checkpoints_require_named_json_values() {
        for source in ["checkpoint(1); return 1;", "checkpoint({n:NaN}); return 1;"] {
            let mut env = Environment::new("{}").unwrap();
            let result = value(env.step("__start", source).unwrap());
            assert_eq!(result["status"], "error");
        }
    }

    #[test]
    fn a_fresh_environment_restores_values_without_replaying_source() {
        let mut first = Environment::new("{}").unwrap();
        let done = value(
            first
                .step("__start", "state.n = 7; checkpoint(state); return state.n;")
                .unwrap(),
        );
        let mut second = Environment::new("{}").unwrap();
        second.restore(&done["checkpoint"]).unwrap();
        let restored = value(
            second
                .step("__start", "return {n:state.n,recovered:context.recovered};")
                .unwrap(),
        );
        assert_eq!(
            restored["value"],
            serde_json::json!({"n":7,"recovered":true})
        );
        assert!(second.step("__resume", r#"{"id":1,"value":null}"#).is_err());
    }

    #[test]
    fn refreshed_context_preserves_working_state_and_recovery() {
        let mut env = Environment::new(r#"{"objective":"old"}"#).unwrap();
        env.restore(&serde_json::json!({"version":1,"state":{"n":7}}))
            .unwrap();
        env.refresh_context(r#"{"turn":2,"job":{"revision":1}}"#)
            .unwrap();
        let done = value(env.step("__start", "return {n:state.n,recovered:context.recovered,turn:context.turn,revision:context.job.revision,objective:context.objective ?? null};").unwrap());
        assert_eq!(
            done["value"],
            serde_json::json!({
                "n":7,"recovered":true,"turn":2,"revision":1,"objective":null
            })
        );
    }

    #[test]
    fn completed_cells_export_serializable_working_values() {
        let mut env = Environment::new("{}").unwrap();
        let done = value(
            env.step(
                "__start",
                "state.answer = 42; checkpoint(state); return state.answer;",
            )
            .unwrap(),
        );
        assert_eq!(done["checkpoint"]["version"], 1);
        assert_eq!(done["checkpoint"]["state"]["answer"], 42);
    }

    #[test]
    fn memory_facade_yields_and_receives_results() {
        let mut env = Environment::new("{}").unwrap();
        for operation in ["recall", "get", "remember", "supersede", "forget"] {
            let call = value(
                env.step(
                    "__start",
                    &format!("return await memory.{operation}({{scope:'project:p'}});"),
                )
                .unwrap(),
            );
            assert_eq!(call["method"], format!("memory.{operation}"));
            assert_eq!(call["args"]["scope"], "project:p");
            let done = value(
                env.step(
                    "__resume",
                    &serde_json::json!({"id":call["id"],"value":{"id":"memory:1"}}).to_string(),
                )
                .unwrap(),
            );
            assert_eq!(done["value"]["id"], "memory:1");
        }
    }
    #[test]
    fn large_results_are_previewed_without_discarding_working_state() {
        let mut env = Environment::new("{}").unwrap();
        let result = env
            .step(
                "__start",
                "state.large = 'x'.repeat(250000); return {sample: state.large};",
            )
            .unwrap();
        assert!(result.len() < 16 * 1024);
        assert!(result.contains("truncated"));
        assert_eq!(
            value(env.step("__start", "return state.large.length;").unwrap())["value"],
            250000
        );
    }

    #[test]
    fn state_survives_cells_and_child_results_resume_computation() {
        let mut env = Environment::new(r#"{"objective":"inspect reports"}"#).unwrap();
        let request = value(
            env.step(
                "__start",
                "state.rows = await history.read({after: 0}); return state.rows.length;",
            )
            .unwrap(),
        );
        assert_eq!(request["method"], "history.read");
        let done = value(
            env.step(
                "__resume",
                &serde_json::json!({"id":request["id"],"value":[{"text":"report"}]}).to_string(),
            )
            .unwrap(),
        );
        assert_eq!(done["value"], 1);
        let request = value(env.step("__start", "state.answer = await rlm.query({question: 'summarize', context: state.rows}); return state.answer;").unwrap());
        assert_eq!(request["args"]["context"][0]["text"], "report");
        let done = value(
            env.step(
                "__resume",
                &serde_json::json!({"id":request["id"],"value":"done"}).to_string(),
            )
            .unwrap(),
        );
        assert_eq!(done["value"], "done");
        assert_eq!(
            value(env.step("__start", "return state.answer;").unwrap())["value"],
            "done"
        );
    }

    #[test]
    fn console_output_reaches_the_cell_result() {
        let mut env = Environment::new("{}").unwrap();

        let done = value(
            env.step(
                "__start",
                "console.log('rows', 3); console.error('bad input'); return 1;",
            )
            .unwrap(),
        );

        assert_eq!(done["value"], 1);
        assert_eq!(done["log"][0], "log: rows 3");
        assert_eq!(done["log"][1], "error: bad input");
    }

    #[test]
    fn console_output_is_bounded() {
        let mut env = Environment::new("{}").unwrap();

        let done = value(
            env.step(
                "__start",
                "for (let i = 0; i < 5000; i++) console.log('x'.repeat(200)); return 1;",
            )
            .unwrap(),
        );

        assert!(
            done["log"].as_array().unwrap().len() < 5000,
            "a runaway loop cannot fill the result with its own output"
        );
        assert!(env.step("__start", "return 1;").unwrap().len() < 4096);
    }

    #[test]
    fn deeply_nested_source_is_refused_before_it_runs() {
        let mut env = Environment::new("{}").unwrap();
        let source = format!("return {}1{};", "(".repeat(500), ")".repeat(500));

        let error = env
            .step("__start", &source)
            .expect_err("parsing is not free: pathological nesting must not reach the parser");

        assert!(error.contains("nests"), "{error}");
    }

    #[test]
    fn oversized_source_is_refused() {
        let mut env = Environment::new("{}").unwrap();
        let source = format!("// {}\nreturn 1;", "x".repeat(128 * 1024));

        let error = env
            .step("__start", &source)
            .expect_err("source has a ceiling");

        assert!(error.contains("bytes"), "{error}");
    }

    #[test]
    fn rejected_host_requests_can_be_handled_by_code() {
        let mut env = Environment::new("{}").unwrap();
        let request = value(
            env.step(
                "__start",
                "try { await capabilities.invoke('bad', {}); } catch (e) { return String(e); }",
            )
            .unwrap(),
        );
        let done = value(
            env.step(
                "__resume",
                &serde_json::json!({"id":request["id"],"error":"denied"}).to_string(),
            )
            .unwrap(),
        );
        assert_eq!(done["value"], "Error: denied");
    }
}
