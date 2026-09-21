#![cfg_attr(not(any(test, target_arch = "wasm32")), allow(dead_code))]
use boa_engine::{Context, JsString, JsValue, Source};
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
        self.set_data("context", &context)
    }

    fn restore(&mut self, checkpoint: &Value) -> Result<(), String> {
        if checkpoint.is_null() {
            return Ok(());
        }
        if checkpoint["version"] != 1 || !checkpoint["state"].is_object() {
            return Err("unsupported working checkpoint".into());
        }
        let mode = match checkpoint.get("mode") {
            None => "explicit",
            Some(Value::String(mode)) if mode == "automatic" || mode == "explicit" => mode.as_str(),
            _ => return Err("unsupported working checkpoint mode".into()),
        };
        let encoded = serde_json::to_string(&checkpoint["state"]).map_err(|e| e.to_string())?;
        if encoded.len() > 32 * 1024 {
            return Err("checkpoint exceeds 32 KiB".into());
        }
        self.set_data("state", &checkpoint["state"])?;
        let mut bridge = checkpoint.clone();
        bridge
            .as_object_mut()
            .expect("checkpoint state was checked as an object")
            .insert("mode".into(), Value::String(mode.into()));
        self.set_data("__checkpoint", &bridge)?;
        self.eval("globalThis.__restoreCheckpoint(__checkpoint); delete globalThis.__checkpoint;")?;
        self.eval("globalThis.context.recovered = true;")?;
        self.recovered = true;
        Ok(())
    }

    fn set_data(&mut self, name: &str, value: &Value) -> Result<(), String> {
        let value = JsValue::from_json(value, &mut self.context).map_err(|e| e.to_string())?;
        self.context
            .global_object()
            .set(JsString::from(name), value, true, &mut self.context)
            .map_err(|e| e.to_string())?;
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
            JsValue::from(JsString::from(input))
        } else {
            let data: Value = serde_json::from_str(input).map_err(|e| e.to_string())?;
            JsValue::from_json(&data, &mut self.context).map_err(|e| e.to_string())?
        };
        let function = self
            .context
            .global_object()
            .get(JsString::from(method), &mut self.context)
            .map_err(|e| e.to_string())?;
        function
            .as_callable()
            .ok_or("invalid environment method")?
            .call(&JsValue::undefined(), &[input], &mut self.context)
            .map_err(|e| e.to_string())?;
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
    use exports::pluribus::plugin::lifecycle::{Context as CallContext, Guest, Outcome};
    use pluribus::plugin::types::{Error, ErrorCode, Event, Payload, Proposal};
    use pluribus_plugin_sdk::export;
    use pluribus_plugin_sdk::{exports, pluribus};
    use serde_json::json;
    use std::cell::RefCell;

    // A running cell is a paused async function, not serializable state. The
    // manifest declares a pinned session so the host keeps this instance for
    // the activity's deliveries; losing it fails the activity.
    thread_local! { static ENV: RefCell<std::collections::BTreeMap<String, Environment>> = const { RefCell::new(std::collections::BTreeMap::new()) }; }

    struct Code;

    use pluribus_plugin_sdk::serve;

    fn setup(_context: CallContext, _config: Vec<u8>) -> Result<Outcome, Error> {
        Ok(empty())
    }

    impl Guest for Code {
        async fn run(
            context: exports::pluribus::plugin::lifecycle::Context,
            config: Vec<u8>,
        ) -> Result<(), Error> {
            let outcome = setup(context.clone(), config)?;
            pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

            serve::<Self>(context).await
        }

        async fn handle(_context: CallContext, events: Vec<Event>) -> Result<Outcome, Error> {
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
                    "warnings": status.get("warnings"),
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
            payload_schema: "dev.pluribus.repl.cell/1".to_owned(),
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
    fn two_suspended_environments_resume_independently() {
        let mut first = Environment::new("{}").unwrap();
        let mut second = Environment::new("{}").unwrap();
        first
            .step("__start", "return await history.read({});")
            .unwrap();
        second.step("__start", "await history.read({}); let total=0; for(let i=0;i<10;i++) { for(let j=0;j<10;j++) { total+=j; } } return total;").unwrap();
        let resumed = value(second.step("__resume", r#"{"id":1,"value":[]}"#).unwrap());
        assert_eq!(resumed["status"], "done", "{resumed}");
        assert_eq!(resumed["value"], 450);
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
    fn explicit_checkpoint_keeps_the_utf8_size_limit() {
        let mut env = Environment::new("{}").unwrap();
        let result = value(
            env.step(
                "__start",
                "checkpoint({text: 'é'.repeat(20000)}); return 1;",
            )
            .unwrap(),
        );

        assert_eq!(result["status"], "error");
        assert!(result["error"].as_str().unwrap().contains("32 KiB"));
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
        assert_eq!(restored["checkpoint"]["mode"], "explicit");
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
        assert_eq!(done["checkpoint"]["mode"], "explicit");
    }

    #[test]
    fn explicit_checkpoint_remains_authoritative_across_cells() {
        let mut env = Environment::new("{}").unwrap();
        let first = value(
            env.step("__start", "checkpoint({selected: 7}); return 1;")
                .unwrap(),
        );
        assert_eq!(first["checkpoint"]["state"]["selected"], 7);
        assert_eq!(first["checkpoint"]["mode"], "explicit");

        let second = value(env.step("__start", "state.other = 9; return 2;").unwrap());
        assert_eq!(second["status"], "done");
        assert_eq!(
            second["checkpoint"]["state"],
            serde_json::json!({"selected": 7})
        );
        assert_eq!(second["checkpoint"]["mode"], "explicit");

        let mut restored = Environment::new("{}").unwrap();
        restored.restore(&first["checkpoint"]).unwrap();
        let after_restart = value(
            restored
                .step("__start", "state.other = 9; return 2;")
                .unwrap(),
        );
        assert_eq!(
            after_restart["checkpoint"]["state"],
            serde_json::json!({"selected": 7})
        );
        assert_eq!(after_restart["checkpoint"]["mode"], "explicit");
    }

    #[test]
    fn successful_cells_automatically_checkpoint_state() {
        let mut env = Environment::new("{}").unwrap();
        let done = value(
            env.step("__start", "state.answer = 42; return 'ok';")
                .unwrap(),
        );

        assert_eq!(done["status"], "done");
        assert_eq!(done["value"], "ok");
        assert_eq!(done["checkpoint"]["version"], 1);
        assert_eq!(done["checkpoint"]["state"]["answer"], 42);
        assert_eq!(done["checkpoint"]["mode"], "automatic");
        assert_eq!(done["warnings"], serde_json::json!([]));
    }

    #[test]
    fn automatic_checkpoint_restores_state_without_replaying_source() {
        let mut first = Environment::new("{}").unwrap();
        let done = value(
            first
                .step("__start", "state.n = (state.n ?? 0) + 1; return state.n;")
                .unwrap(),
        );
        assert_eq!(done["checkpoint"]["mode"], "automatic");
        let mut second = Environment::new("{}").unwrap();
        second.restore(&done["checkpoint"]).unwrap();

        let restored = value(second.step("__start", "return state.n;").unwrap());
        assert_eq!(restored["value"], 1);
        assert_eq!(restored["checkpoint"]["mode"], "automatic");
    }

    #[test]
    fn invalid_automatic_state_warns_without_failing_completed_cell() {
        let mut env = Environment::new("{}").unwrap();
        let done = value(
            env.step(
                "__start",
                "state.answer = 42; state.bad = undefined; return state.answer;",
            )
            .unwrap(),
        );

        assert_eq!(done["status"], "done");
        assert_eq!(done["value"], 42);
        assert!(done["checkpoint"].is_null());
        assert_eq!(done["warnings"].as_array().unwrap().len(), 1);
        assert!(
            done["warnings"][0]
                .as_str()
                .unwrap()
                .contains("unsupported")
        );
    }

    #[test]
    fn automatic_checkpoint_counts_utf8_bytes() {
        let mut env = Environment::new("{}").unwrap();
        let done = value(
            env.step("__start", "state.text = 'é'.repeat(20000); return 1;")
                .unwrap(),
        );

        assert_eq!(done["status"], "done");
        assert_eq!(done["value"], 1);
        assert!(done["checkpoint"].is_null());
        assert!(done["warnings"][0].as_str().unwrap().contains("32 KiB"));
    }

    #[test]
    fn automatic_checkpoint_rejects_cycles_and_nonfinite_values() {
        for source in [
            "state.self = state; return 1;",
            "state.nan = NaN; return 1;",
            "state.infinity = Infinity; return 1;",
            "state.fn = () => 1; return 1;",
            "state.symbol = Symbol('x'); return 1;",
        ] {
            let mut env = Environment::new("{}").unwrap();
            let done = value(env.step("__start", source).unwrap());
            assert_eq!(done["status"], "done", "{source}");
            assert_eq!(done["value"], 1, "{source}");
            assert!(done["checkpoint"].is_null(), "{source}");
            assert_eq!(done["warnings"].as_array().unwrap().len(), 1, "{source}");
        }
    }

    #[test]
    fn automatic_checkpoint_duplicates_shared_values_without_calling_them_cycles() {
        let mut env = Environment::new("{}").unwrap();
        let done = value(
            env.step(
                "__start",
                "const item = {answer: 42}; state.first = item; state.second = item; return 1;",
            )
            .unwrap(),
        );

        assert_eq!(done["status"], "done");
        assert_eq!(
            done["checkpoint"]["state"]["first"],
            serde_json::json!({"answer": 42})
        );
        assert_eq!(
            done["checkpoint"]["state"]["second"],
            serde_json::json!({"answer": 42})
        );
    }

    #[test]
    fn automatic_checkpoint_preserves_an_own_proto_key_as_data() {
        let mut env = Environment::new("{}").unwrap();
        let done = value(
            env.step(
                "__start",
                "Object.defineProperty(state, '__proto__', {value: {injected: true}, enumerable: true}); return 1;",
            )
            .unwrap(),
        );

        assert_eq!(done["status"], "done");
        assert_eq!(done["checkpoint"]["state"]["__proto__"]["injected"], true);
    }

    #[test]
    fn json_objects_preserve_proto_keys_as_data() {
        let mut env = Environment::new(r#"{"__proto__":{"injected":true}}"#).unwrap();
        let result = value(env.step("__start", "return {own:Object.hasOwn(context,'__proto__'), inherited:context.injected === true};").unwrap());
        assert_eq!(
            result["value"],
            serde_json::json!({"own":true,"inherited":false})
        );
        let request = value(env.step("__start", "const x=await history.read({}); return {own:Object.hasOwn(x,'__proto__'), inherited:x.injected === true};").unwrap());
        let result = value(
            env.step(
                "__resume",
                &serde_json::json!({"id":request["id"],"value":{"__proto__":{"injected":true}}})
                    .to_string(),
            )
            .unwrap(),
        );
        assert_eq!(
            result["value"],
            serde_json::json!({"own":true,"inherited":false})
        );
    }

    #[test]
    fn plugin_boundary_has_no_sibling_globals() {
        let mut env = Environment::new("{}").unwrap();
        let done = value(env.step("__start", "return typeof memory;").unwrap());
        assert_eq!(done["value"], "undefined");
    }

    #[test]
    fn capability_facade_yields_and_receives_receipts() {
        let mut env = Environment::new("{}").unwrap();
        for operation in ["catalog.search", "catalog.update"] {
            let call = value(
                env.step(
                    "__start",
                    &format!(
                        "return await capabilities.invoke('{operation}', {{scope:'project:p'}});"
                    ),
                )
                .unwrap(),
            );
            assert_eq!(call["method"], "capability.invoke");
            assert_eq!(call["args"]["name"], operation);
            assert_eq!(call["args"]["arguments"]["scope"], "project:p");
            let done = value(
                env.step(
                    "__resume",
                    &serde_json::json!({"id":call["id"],"value":{"output":{"id":"record:1"}}})
                        .to_string(),
                )
                .unwrap(),
            );
            assert_eq!(done["value"]["output"]["id"], "record:1");
        }
    }

    #[test]
    fn history_search_facade_yields_to_the_host() {
        let mut env = Environment::new("{}").unwrap();
        let request = value(
            env.step(
                "__start",
                "return await history.search({query: 'deployment'});",
            )
            .unwrap(),
        );
        assert_eq!(request["method"], "history.search");
        assert_eq!(request["args"]["query"], "deployment");
        let done = value(
            env.step(
                "__resume",
                &serde_json::json!({"id":request["id"],"value":{"events":[]}}).to_string(),
            )
            .unwrap(),
        );
        assert_eq!(done["value"]["events"], serde_json::json!([]));
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
