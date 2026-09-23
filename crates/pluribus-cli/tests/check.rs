use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn check(config: &Value) -> std::process::Output {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join("config");
    fs::create_dir(&config_dir).unwrap();
    let bytes = serde_json::to_vec(config).unwrap();
    fs::write(config_dir.join("config.json"), &bytes).unwrap();
    let state = temp.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_pluribus"))
        .env(
            "PLURIBUS_PLUGIN_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins"),
        )
        .arg("--config-dir")
        .arg(&config_dir)
        .arg("--data-dir")
        .arg(&state)
        .arg("--cache-dir")
        .arg(temp.path().join("cache"))
        .arg("--runtime-dir")
        .arg(temp.path().join("runtime"))
        .args(["check", "--offline"])
        .output()
        .unwrap();
    assert!(!state.exists(), "configuration checks must not open state");
    assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), bytes);
    output
}

fn configuration() -> Value {
    json!({
        "plugin_instances":{"memory":{"package":"bundled:memory"}},
        "capability_instances":["memory"],
        "trusted_capabilities":{"memory":["memory.recall"]},
        "trusted_constraints":{"memory":{"memory.recall":{"scopes":["personal"]}}}
    })
}

#[test]
fn check_validates_packages_without_starting_plugins_or_writing_state() {
    let output = check(&configuration());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_rejects_grants_absent_from_the_selected_provider() {
    let mut config = configuration();
    config["trusted_capabilities"]["memory"] = json!(["memory.missing"]);
    config["trusted_constraints"] = json!({});
    let output = check(&config);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("memory.missing"), "{error}");
}

#[test]
fn check_rejects_constraints_outside_the_manifest_schema() {
    let mut config = configuration();
    config["trusted_constraints"]["memory"]["memory.recall"] = json!({"scopes": "personal"});
    let output = check(&config);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("constraints") && error.contains("memory.recall"),
        "{error}"
    );
}

#[test]
fn check_rejects_a_model_absent_from_the_selected_provider() {
    let config = json!({
        "model":"missing-model", "model_instance":"codex",
        "plugin_instances":{"codex":{"package":"bundled:openai-codex"},"scheduler":{"package":"bundled:scheduler"}}
    });
    let output = check(&config);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("missing-model"), "{error}");
}

#[test]
fn check_rejects_timer_producers_without_a_timer_executor() {
    let config = json!({"plugin_instances":{"codex":{"package":"bundled:openai-codex"}}});
    let output = check(&config);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("timer.fired"), "{error}");
}
