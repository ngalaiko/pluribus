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
            "package": url::Url::from_directory_path(&package).unwrap().as_str(), "config": {"credential_handle": handle},
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
fn external_manifest_does_not_grant_network_access() {
    let data = TempDir::new().unwrap();
    let mut config = external_config(data.path());
    config["plugin_instances"]["work"]["components"]["receive"]["http"]["origins"] = json!([]);
    let path = data.path().join("config.json");
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let result = cli(
        data.path(),
        &["auth", "work"],
        Some("123456:fake-test-token-00000000\n"),
    );
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("fake-test-token"));
    config["plugin_instances"]["work"]["components"]["receive"]["http"]["origins"] =
        json!(["https://api.telegram.org"]);
    config["plugin_instances"]["work"]["components"]["receive"]["http"]["methods"] =
        json!(["DELETE"]);
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let result = cli(
        data.path(),
        &["auth", "work"],
        Some("123456:fake-test-token-00000000\n"),
    );
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("HTTP grant exceeds manifest"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    config["plugin_instances"]["work"]["components"]["receive"]["http"]["methods"] =
        json!(["POST"]);
    config["plugin_instances"]["work"]["enrollment_origins"] =
        json!(["https://ungranted.example.com"]);
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let result = cli(data.path(), &["auth", "work"], None);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("enrollment grant exceeds manifest"));
}
