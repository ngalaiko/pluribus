use serde_json::{Value, json};
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn cli(data: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pluribus"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn multi_credential_config(data: &Path) -> Value {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram");
    let package = data.join("plugins/multi");
    copy_tree(&source, &package);
    let manifest = fs::read_to_string(source.join("plugin.toml"))
        .unwrap()
        .replace("dev.pluribus.telegram", "dev.example.multi");
    fs::write(
        package.join("plugin.toml"),
        format!(
            "{manifest}\n[[credentials]]\ncomponents = [\"receive\", \"send\"]\nid = \"second-token\"\naccess = true\ndisplay_name = \"Second token\"\ndescription = \"A second token\"\ninput_schema = \"schemas/credential.input.json\"\nflow_schema = \"pluribus:credential/static-plugin@1\"\nflow = \"flows/bot-token.json\"\n"
        ),
    )
    .unwrap();
    let mut schema: Value =
        serde_json::from_slice(&fs::read(package.join("config.schema.json")).unwrap()).unwrap();
    schema["properties"]["credentials"]["properties"]["second-token"] =
        json!({"type": "string", "minLength": 1});
    fs::write(
        package.join("config.schema.json"),
        serde_json::to_vec_pretty(&schema).unwrap(),
    )
    .unwrap();
    let mut credential_schema: Value =
        serde_json::from_slice(&fs::read(package.join("schemas/credential.input.json")).unwrap())
            .unwrap();
    credential_schema["properties"]["token"]["writeOnly"] = json!(false);
    fs::write(
        package.join("schemas/credential.input.json"),
        serde_json::to_vec_pretty(&credential_schema).unwrap(),
    )
    .unwrap();
    json!({
        "agent_id": "test", "identity": "test", "model": "test",
        "maximum_blob_bytes": 1024,
        "plugin_instances": {
            "work": {
                "package": url::Url::from_directory_path(&package).unwrap().as_str(),
                "config": {"credentials": {"bot-token": "test:first", "second-token": "test:second"}},
                "components": {
                    "receive": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}},
                    "send": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}}
                }
            }
        }
    })
}

fn inaccessible_plugin_config(data: &Path) -> Value {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/openai-codex");
    let package = data.join("plugins/inaccessible");
    copy_tree(&source, &package);
    let manifest = fs::read_to_string(source.join("plugin.toml"))
        .unwrap()
        .replace("dev.pluribus.openai-codex", "dev.example.inaccessible")
        .replace("access = true", "access = false");
    fs::write(package.join("plugin.toml"), manifest).unwrap();
    json!({
        "agent_id": "test", "identity": "test", "model": "test",
        "maximum_blob_bytes": 1024,
        "plugin_instances": {
            "work": {
                "package": url::Url::from_directory_path(&package).unwrap().as_str(),
                "config": {"credentials": {"subscription": "test:codex"}, "models": ["test"]},
                "components": {
                    "main": {"http": {"origins": ["https://auth.openai.com", "https://chatgpt.com"], "methods": ["POST"]}}
                }
            }
        }
    })
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Copies a package directory, including the schema and flow files its
/// manifest names.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn external_config(data: &Path) -> Value {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram");
    let package = data.join("plugins/external");
    copy_tree(&source, &package);
    let manifest = fs::read_to_string(source.join("plugin.toml"))
        .unwrap()
        .replace("dev.pluribus.telegram", "dev.example.external");
    fs::write(package.join("plugin.toml"), manifest).unwrap();
    let instance = |handle: &str, aliases: &[&str]| {
        json!({
            "package": url::Url::from_directory_path(&package).unwrap().as_str(), "config": {"credentials": {"bot-token": handle}},
            "aliases": aliases,
            "components": {
                "receive": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}},
                "send": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}}
            }
        })
    };
    json!({
        "agent_id": "test", "identity": "test", "model": "test",
        "maximum_blob_bytes": 1024,
        "plugin_instances": {
            "work": instance("test:work", &["office"]),
            "home": instance("test:home", &[])
        }
    })
}

#[test]
fn external_package_auth_uses_configured_instances_and_aliases() {
    let data = TempDir::new().unwrap();
    let config = external_config(data.path());
    let path = data.path().join("config.json");
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    // An alias names the instance it belongs to.
    success(cli(
        data.path(),
        &["auth", "office"],
        Some("123456:fake-test-token-00000000\n"),
    ));
    let unknown = cli(data.path(), &["auth", "telegram"], None);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown plugin instance"));
}

#[test]
fn a_grant_wider_than_the_manifest_is_refused() {
    let data = TempDir::new().unwrap();
    let mut config = external_config(data.path());
    let path = data.path().join("config.json");
    config["plugin_instances"]["work"]["components"]["receive"]["http"]["methods"] =
        json!(["DELETE"]);
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let result = cli(
        data.path(),
        &["auth", "work"],
        Some("123456:fake-test-token-00000000\n"),
    );
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("fake-test-token"));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("HTTP grant exceeds manifest"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn interactive_auth_uses_the_selected_credential() {
    let data = TempDir::new().unwrap();
    let config = multi_credential_config(data.path());
    fs::write(
        data.path().join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_pluribus");
    let script = format!(
        r#"
set timeout 2
spawn {bin} --data-dir {data} auth work
expect "1. Telegram bot token"
send "2\r"
expect "Bot token"
send "123456:second-test-token-00000000\r"
expect "Second token stored."
"#,
        bin = bin,
        data = data.path().display(),
    );
    let output = Command::new("expect")
        .args(["-c", &script])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let database = data.path().join("pluribus.sqlite3");
    let stored = sqlite_value(&database, "test:second", "dev.example.multi");
    assert_eq!(stored["token"], "123456:second-test-token-00000000");
    assert!(sqlite_value_opt(&database, "test:first", "dev.example.multi").is_none());
}

#[test]
fn plugin_auth_rejects_an_inaccessible_credential_before_waiting() {
    let data = TempDir::new().unwrap();
    let config = inaccessible_plugin_config(data.path());
    fs::write(
        data.path().join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pluribus"))
        .arg("--data-dir")
        .arg(data.path())
        .args(["auth", "work"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let status = child.try_wait().unwrap();
    if status.is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("credential is not readable by any component"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sqlite_value(path: &Path, handle: &str, provider: &str) -> Value {
    let value = sqlite_value_opt(path, handle, provider).unwrap();
    serde_json::from_slice(&value).unwrap()
}

fn sqlite_value_opt(path: &Path, handle: &str, provider: &str) -> Option<Vec<u8>> {
    let output = Command::new("sqlite3")
        .args([
            path.to_str().unwrap(),
            &format!(
                "SELECT hex(value) FROM plugin_credentials WHERE handle='{handle}' AND provider='{provider}'"
            ),
        ])
        .output()
        .unwrap();
    let hex = String::from_utf8(output.stdout).unwrap();
    let hex = hex.trim();
    if hex.is_empty() {
        return None;
    }
    Some(
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect(),
    )
}
