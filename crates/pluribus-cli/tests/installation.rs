use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}

#[test]
fn explicit_file_sources_survive_binary_relocation() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("installation");
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_pluribus"), prefix.join("bin/pluribus")).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    copy_tree(
        &source.join("memory"),
        &prefix.join("share/pluribus/plugins/memory"),
    );
    let data = temp.path().join("data");
    let invoke = |prefix: &Path, arguments: &[&str]| {
        Command::new(prefix.join("bin/pluribus"))
            .current_dir(temp.path())
            .env_remove("PLURIBUS_PLUGIN_DIR")
            .arg("--data-dir")
            .arg(&data)
            .arg("--config-dir")
            .arg(&data)
            .arg("--cache-dir")
            .arg(&data)
            .arg("--runtime-dir")
            .arg(&data)
            .args(arguments)
            .output()
            .unwrap()
    };
    let output = invoke(&prefix, &["init"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config_path = data.join("config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(
        config["plugin_instances"],
        json!({}),
        "init installs nothing"
    );

    // An explicit file source, the way a configuration names an installed
    // package.
    let package = url::Url::from_directory_path(
        prefix
            .join("share/pluribus/plugins/memory")
            .canonicalize()
            .unwrap(),
    )
    .unwrap();
    config["plugin_instances"]["memory-1"] = json!({
        "package": package.as_str(),
        "config": {},
        "components": {"main": {}}
    });
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let bytes = fs::read(&config_path).unwrap();

    let moved = temp.path().join("moved");
    fs::create_dir(&moved).unwrap();
    fs::rename(prefix.join("bin"), moved.join("bin")).unwrap();
    // Reaching the credential check means the package resolved from where the
    // configuration names it, not from beside the binary.
    let error = String::from_utf8_lossy(&invoke(&moved, &["plugins", "auth", "memory-1"]).stderr)
        .into_owned();
    assert!(error.contains("declares no credentials"), "{error}");
    assert_eq!(fs::read(&config_path).unwrap(), bytes);
}

#[test]
fn missing_installed_packages_do_not_fall_back_to_the_build_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("pluribus");
    fs::copy(env!("CARGO_BIN_EXE_pluribus"), &binary).unwrap();
    let output = Command::new(binary)
        .env_remove("PLURIBUS_PLUGIN_DIR")
        .arg("--data-dir")
        .arg(temp.path().join("data"))
        .arg("--config-dir")
        .arg(temp.path().join("data"))
        .arg("--cache-dir")
        .arg(temp.path().join("data"))
        .arg("--runtime-dir")
        .arg(temp.path().join("data"))
        .args(["init", "--example"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no source for package"), "{stderr}");
    assert!(!stderr.contains(env!("CARGO_MANIFEST_DIR")), "{stderr}");
}

#[test]
fn installing_a_package_directory_configures_it_without_rust() {
    let temp = tempfile::tempdir().unwrap();
    let bundle = temp.path().join("bundle");
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_pluribus"), bundle.join("bin/pluribus")).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    for name in ["memory", "rlm"] {
        copy_tree(
            &source.join(name),
            &bundle.join("share/pluribus/plugins").join(name),
        );
    }
    let data = temp.path().join("data");
    let invoke = |arguments: &[&str]| {
        Command::new(bundle.join("bin/pluribus"))
            .env("PATH", "")
            .env_remove("PLURIBUS_PLUGIN_DIR")
            .arg("--data-dir")
            .arg(&data)
            .arg("--config-dir")
            .arg(&data)
            .arg("--cache-dir")
            .arg(&data)
            .arg("--runtime-dir")
            .arg(&data)
            .args(arguments)
            .output()
            .unwrap()
    };
    assert!(invoke(&["init"]).status.success());

    let package = bundle.join("share/pluribus/plugins/memory");
    let output = invoke(&["plugins", "install", &package.to_string_lossy()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: Value =
        serde_json::from_slice(&fs::read(data.join("config.json")).unwrap()).unwrap();
    let configured = config["plugin_instances"]["memory"]["package"]
        .as_str()
        .unwrap();
    assert!(configured.starts_with("file://"), "{configured}");
    assert!(configured.ends_with("memory/"), "{configured}");
    assert_eq!(
        config["plugin_instances"]["memory"]
            .as_object()
            .unwrap()
            .len(),
        1
    );

    // The same package cannot be installed twice under one name.
    assert!(
        !invoke(&["plugins", "install", &package.to_string_lossy()])
            .status
            .success()
    );
}

#[test]
fn bundled_references_resolve_through_the_current_installation() {
    let temp = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    let install = |prefix: &Path| {
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_pluribus"), prefix.join("bin/pluribus")).unwrap();
        copy_tree(
            &source.join("memory"),
            &prefix.join("share/pluribus/plugins/memory"),
        );
    };
    let old = temp.path().join("old");
    install(&old);
    let data = temp.path().join("data");
    let invoke = |prefix: &Path, arguments: &[&str]| {
        Command::new(prefix.join("bin/pluribus"))
            .env_remove("PLURIBUS_PLUGIN_DIR")
            .arg("--data-dir")
            .arg(&data)
            .arg("--config-dir")
            .arg(&data)
            .arg("--cache-dir")
            .arg(&data)
            .arg("--runtime-dir")
            .arg(&data)
            .args(arguments)
            .output()
            .unwrap()
    };
    assert!(invoke(&old, &["init"]).status.success());

    let config_path = data.join("config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["plugin_instances"]["memory"] = json!({
        "package": "bundled:memory",
        "config": {},
        "components": {"main": {}}
    });
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    // Reaching the credential check means the name resolved.
    let resolved = |prefix: &Path| {
        let error = String::from_utf8_lossy(&invoke(prefix, &["plugins", "auth", "memory"]).stderr)
            .into_owned();
        assert!(error.contains("declares no credentials"), "{error}");
    };
    resolved(&old);

    // An upgrade replaces the installation; the reference follows it.
    let new = temp.path().join("new");
    install(&new);
    fs::remove_dir_all(&old).unwrap();
    resolved(&new);
}
