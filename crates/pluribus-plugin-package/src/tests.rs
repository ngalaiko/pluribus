use super::*;
use serde_json::json;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};

static NEXT_PACKAGE: AtomicU64 = AtomicU64::new(1);

#[test]
fn bundle_uses_one_package_abi_and_rejects_component_overrides() {
    let fixture = TestPackage::create();
    let mut manifest: toml::Value = toml::from_str(&fixture.manifest()).unwrap();
    assert_eq!(manifest["abi"].as_str(), Some("pluribus:plugin@1.0.0"));
    let mut sibling = manifest["components"]["main"].clone();
    fs::copy(
        fixture.path.join("plugin.wasm"),
        fixture.path.join("sibling.wasm"),
    )
    .unwrap();
    sibling["component"] = toml::Value::String("sibling.wasm".into());
    sibling["provides"] = toml::Value::Array(Vec::new());
    manifest["components"]
        .as_table_mut()
        .unwrap()
        .insert("sibling".into(), sibling);
    fixture.set_manifest(&toml::to_string(&manifest).unwrap());
    assert_eq!(
        PluginPackage::load(&fixture.path)
            .unwrap()
            .components()
            .len(),
        2
    );

    for abi in ["pluribus:plugin@1.0.0", "pluribus:plugin@0.1.0"] {
        manifest["components"]["sibling"]
            .as_table_mut()
            .unwrap()
            .insert("abi".into(), toml::Value::String(abi.into()));
        fixture.set_manifest(&toml::to_string(&manifest).unwrap());
        assert!(PluginPackage::load(&fixture.path).is_err());
    }
}

const IMPORTS: &[&str] = &[
    "pluribus:plugin/events@1.0.0",
    "pluribus:plugin/state@1.0.0",
    "pluribus:plugin/blobs@1.0.0",
    "pluribus:plugin/reader@1.0.0",
    "pluribus:plugin/writer@1.0.0",
    "pluribus:plugin/http@1.0.0",
    "pluribus:plugin/socket@1.0.0",
    "pluribus:plugin/credentials@1.0.0",
];

struct TestPackage {
    path: PathBuf,
}

impl TestPackage {
    fn create() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pluribus-plugin-package-{}-{}",
            std::process::id(),
            NEXT_PACKAGE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();

        let component = fixture_component();
        fs::write(path.join("plugin.wasm"), &component).unwrap();
        fs::write(
            path.join("config.schema.json"),
            br#"{
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "properties": { "label": { "type": "string" } }
            }"#,
        )
        .unwrap();
        fs::write(path.join("plugin.toml"), fixture_manifest(&component)).unwrap();

        Self { path }
    }

    fn manifest(&self) -> String {
        fs::read_to_string(self.path.join("plugin.toml")).unwrap()
    }

    fn set_manifest(&self, manifest: &str) {
        fs::write(self.path.join("plugin.toml"), manifest).unwrap();
    }
}

impl Drop for TestPackage {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[test]
fn valid_package_and_configuration_load() {
    let package = TestPackage::create();

    let loaded = PluginPackage::load(&package.path).unwrap();

    assert_eq!(loaded.manifest().id, "dev.example.fixture");
    assert_eq!(
        loaded.component("main").unwrap().manifest().provides[0].capability,
        "fixture.echo"
    );
    assert!(!loaded.component("main").unwrap().component().is_empty());
    assert!(
        loaded
            .validate_config(&json!({ "label": "office" }))
            .is_ok()
    );
    assert!(loaded.validate_config(&json!({ "unknown": true })).is_err());
}

#[test]
fn digest_mismatch_is_rejected() {
    let package = TestPackage::create();
    let manifest = package.manifest();
    let digest = manifest
        .lines()
        .find(|line| line.starts_with("digest = "))
        .unwrap();
    package.set_manifest(
        &manifest.replace(digest, &format!("digest = \"sha256:{}\"", "0".repeat(64))),
    );

    let error = PluginPackage::load(&package.path).err().unwrap();

    assert!(error.to_string().contains("component digest mismatch"));
}

#[test]
fn malformed_component_is_rejected() {
    let package = TestPackage::create();
    let bytes = b"not a component";
    fs::write(package.path.join("plugin.wasm"), bytes).unwrap();
    let manifest = package.manifest();
    let digest = manifest
        .lines()
        .find(|line| line.starts_with("digest = "))
        .unwrap();
    package.set_manifest(&manifest.replace(digest, &format!("digest = \"{}\"", sha256(bytes))));

    let error = PluginPackage::load(&package.path).err().unwrap();

    assert!(error.to_string().contains("invalid WebAssembly component"));
}

#[test]
fn a_foreign_abi_is_rejected() {
    let package = TestPackage::create();
    package.set_manifest(&package.manifest().replace(
        "abi = \"pluribus:plugin@1.0.0\"",
        "abi = \"pluribus:plugin@0.1.0\"",
    ));

    let error = PluginPackage::load(&package.path).err().unwrap();

    assert!(
        error.to_string().contains("pluribus:plugin@1.0.0"),
        "{error}"
    );
}

#[test]
fn imports_must_match_component() {
    let package = TestPackage::create();
    package.set_manifest(
        &package
            .manifest()
            .replace("  \"pluribus:plugin/socket@1.0.0\",\n", ""),
    );

    let error = PluginPackage::load(&package.path).err().unwrap();

    assert!(error.to_string().contains("component imports mismatch"));
}

#[test]
fn unknown_manifest_fields_are_rejected() {
    let package = TestPackage::create();
    package.set_manifest(&format!("{}\nroot = true\n", package.manifest()));

    let error = PluginPackage::load(&package.path).err().unwrap();

    assert!(error.to_string().contains("invalid plugin manifest"));
}

#[cfg(unix)]
#[test]
fn symlinked_package_files_are_rejected() {
    use std::os::unix::fs::symlink;

    let package = TestPackage::create();
    let target = package.path.join("actual.wasm");
    fs::rename(package.path.join("plugin.wasm"), &target).unwrap();
    symlink(&target, package.path.join("plugin.wasm")).unwrap();

    let error = PluginPackage::load(&package.path).err().unwrap();

    assert!(error.to_string().contains("contains a symlink"));
}

#[test]
fn package_builder_produces_an_activatable_component() {
    let template = TestPackage::create();
    let manifest = template.manifest();
    let digest = manifest
        .lines()
        .find(|line| line.starts_with("digest = "))
        .unwrap();
    template.set_manifest(&manifest.replace(digest, "digest = \"sha256:dev\""));
    fs::write(template.path.join("plugin.wasm"), fixture_module()).unwrap();
    let output = std::env::temp_dir().join(format!(
        "pluribus-plugin-output-{}-{}",
        std::process::id(),
        NEXT_PACKAGE.fetch_add(1, Ordering::Relaxed)
    ));

    let digest = build_component_package(
        &template.path,
        &BTreeMap::from([("main".into(), template.path.join("plugin.wasm"))]),
        &output,
    )
    .unwrap();
    let package = PluginPackage::load(&output).unwrap();

    assert_eq!(
        digest["main"],
        package.component("main").unwrap().manifest().digest
    );
    fs::remove_dir_all(output).unwrap();
}

fn fixture_manifest(component: &[u8]) -> String {
    let imports = IMPORTS
        .iter()
        .map(|import| format!("  \"{import}\","))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"manifest_version = 1
abi = "pluribus:plugin@1.0.0"
id = "dev.example.fixture"
name = "Fixture"
config_schema = "config.schema.json"

[components.main]
world = "pluribus:plugin/plugin@1.0.0"
component = "plugin.wasm"
digest = "{}"
imports = [
{}
]
config_schema = "config.schema.json"
emits = ["capability.completed"]

[[components.main.provides]]
capability = "fixture.echo"
description = "Fixture capability."
arguments_schema = "config.schema.json"
result_schema = "config.schema.json"
idempotency = "inherent"
"#,
        sha256(component),
        imports
    )
}

fn fixture_component() -> Vec<u8> {
    ComponentEncoder::default()
        .module(&fixture_module())
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

fn fixture_module() -> Vec<u8> {
    let wit = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../wit");
    let mut resolve = Resolve::default();
    let (package, _) = resolve.push_dir(wit).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    module
}

fn add_second_component(package: &TestPackage) {
    fs::copy(
        package.path.join("plugin.wasm"),
        package.path.join("second.wasm"),
    )
    .unwrap();
    let manifest = package.manifest();
    let second = manifest
        .split("[components.main]")
        .nth(1)
        .unwrap()
        .replace("plugin.wasm", "second.wasm")
        .replace("components.main", "components.second")
        .replace("fixture.echo", "fixture.second");
    package.set_manifest(&format!("{manifest}\n[components.second]{second}"));
}

#[test]
fn bundle_validates_full_configuration_for_all_components() {
    let package = TestPackage::create();
    add_second_component(&package);
    let manifest = package.manifest();
    package.set_manifest(&manifest.replacen(
        "config_schema = \"config.schema.json\"",
        "config_schema = \"package.schema.json\"",
        1,
    ));
    fs::write(
        package.path.join("package.schema.json"),
        b"{\"type\":\"object\"}",
    )
    .unwrap();
    let loaded = PluginPackage::load(&package.path).unwrap();
    assert_eq!(loaded.components().len(), 2);
    assert!(loaded.component("unknown").is_none());
    assert!(loaded.validate_config(&json!({"label":"ok"})).is_ok());
    assert!(loaded.validate_config(&json!({"label":7})).is_err());
    assert!(loaded.validate_config(&json!({})).is_ok());
}

#[test]
fn invalid_later_component_rejects_entire_bundle() {
    let package = TestPackage::create();
    add_second_component(&package);
    fs::write(package.path.join("second.wasm"), b"bad").unwrap();
    assert!(
        PluginPackage::load(&package.path)
            .err()
            .unwrap()
            .to_string()
            .contains("digest mismatch")
    );
}

#[test]
fn bundle_rejects_duplicate_capability_and_unknown_dependencies() {
    let package = TestPackage::create();
    add_second_component(&package);
    let manifest = package.manifest();
    package.set_manifest(&manifest.replace("fixture.second", "fixture.echo"));
    assert!(
        PluginPackage::load(&package.path)
            .err()
            .unwrap()
            .to_string()
            .contains("duplicate capability")
    );
    package.set_manifest(&manifest.replace(
        "[components.second]",
        "[components.second]\nrequires = [\"missing\"]",
    ));
    assert!(
        PluginPackage::load(&package.path)
            .err()
            .unwrap()
            .to_string()
            .contains("invalid component requirement")
    );
}

#[test]
fn bundle_rejects_unsafe_later_schema_and_credential_reference() {
    let package = TestPackage::create();
    add_second_component(&package);
    let manifest = package.manifest();
    package.set_manifest(&manifest.replace(
        "arguments_schema = \"config.schema.json\"",
        "arguments_schema = \"../escape.json\"",
    ));
    assert!(PluginPackage::load(&package.path).is_err());
    package.set_manifest(&format!("{manifest}\n[[credentials]]\ncomponents = [\"missing\"]\nid=\"token\"\ndisplay_name=\"Token\"\ndescription=\"Token\"\ninput_schema=\"config.schema.json\"\nflow_schema=\"pluribus:credential/api-key@1\"\nflow=\"config.schema.json\"\n"));
    assert!(
        PluginPackage::load(&package.path)
            .err()
            .unwrap()
            .to_string()
            .contains("unknown credential component")
    );
}

#[test]
fn a_foreign_manifest_version_is_rejected() {
    let package = TestPackage::create();
    package.set_manifest(
        &package
            .manifest()
            .replace("manifest_version = 1", "manifest_version = 0"),
    );
    assert!(PluginPackage::load(&package.path).is_err());
}

#[test]
fn builder_requires_every_named_module_before_writing() {
    let template = TestPackage::create();
    add_second_component(&template);
    let output = template.path.join("output");
    assert!(build_component_package(&template.path, &BTreeMap::new(), &output).is_err());
    assert!(!output.exists());
}

#[cfg(unix)]
#[test]
fn builder_rejects_dangling_output_symlink_without_writing_target() {
    let template = TestPackage::create();
    let manifest = template.manifest();
    let digest = sha256(&fixture_component());
    template.set_manifest(&manifest.replace(&digest, "sha256:dev"));
    let module = template.path.join("module.wasm");
    fs::write(&module, fixture_module()).unwrap();
    let output = template.path.join("output");
    fs::create_dir(&output).unwrap();
    let target = template.path.join("untouched.wasm");
    std::os::unix::fs::symlink(&target, output.join("plugin.wasm")).unwrap();
    let result = build_component_package(
        &template.path,
        &BTreeMap::from([("main".into(), module)]),
        &output,
    );
    assert!(result.is_err());
    assert!(!target.exists(), "builder followed a dangling symlink");
}

#[test]
fn shipped_single_component_packages_are_version_one() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    for name in ["memory", "shell", "openrouter", "openai-codex"] {
        let package = PluginPackage::load(root.join(name)).unwrap();
        assert_eq!(package.manifest().manifest_version, 1);
        assert_eq!(package.components().len(), 1);
        assert!(package.components().values().next().is_some());
    }
}

#[test]
fn builder_validates_every_module_before_publishing_bundle() {
    let template = TestPackage::create();
    add_second_component(&template);
    template.set_manifest(
        &template
            .manifest()
            .replace(&sha256(&fixture_component()), "sha256:dev"),
    );
    let module = template.path.join("module.wasm");
    let later = template.path.join("later.wasm");
    fs::write(&module, fixture_module()).unwrap();
    fs::write(&later, b"invalid module").unwrap();
    let modules = BTreeMap::from([("main".into(), module), ("second".into(), later.clone())]);
    let output = template.path.join("output");
    assert!(build_component_package(&template.path, &modules, &output).is_err());
    assert!(!output.exists());
    fs::write(&later, fixture_module()).unwrap();
    let digests = build_component_package(&template.path, &modules, &output).unwrap();
    let loaded = PluginPackage::load(&output).unwrap();
    assert_eq!(digests.len(), 2);
    for (name, component) in loaded.components() {
        assert_eq!(digests[name], component.manifest().digest);
    }
}

#[test]
fn component_dependency_cycles_are_rejected() {
    let package = TestPackage::create();
    add_second_component(&package);
    package.set_manifest(
        &package
            .manifest()
            .replace(
                "[components.main]",
                "[components.main]\nrequires=[\"second\"]",
            )
            .replace(
                "[components.second]",
                "[components.second]\nrequires=[\"main\"]",
            ),
    );
    assert!(
        PluginPackage::load(&package.path)
            .err()
            .unwrap()
            .to_string()
            .contains("cyclic")
    );
}

#[test]
fn single_component_needs_no_name() {
    let fixture = TestPackage::create();
    fixture.set_manifest(&fixture.manifest().replace("components.main", "component"));
    let package = PluginPackage::load(&fixture.path).unwrap();
    assert!(package.component("").is_some());
    assert_eq!(package.components().len(), 1);
}

#[test]
fn package_defaults_merge_without_overwriting_operator_values() {
    let fixture = TestPackage::create();
    fixture.set_manifest(&format!(
        "{}\n[defaults]\nlabel = \"default\"\n",
        fixture.manifest()
    ));
    let package = PluginPackage::load(&fixture.path).unwrap();
    assert_eq!(
        package.resolve_config(&json!({})),
        json!({"label":"default"})
    );
    assert_eq!(
        package.resolve_config(&json!({"label":"custom"})),
        json!({"label":"custom"})
    );
}

#[test]
fn builder_supports_an_unnamed_component() {
    let template = TestPackage::create();
    template.set_manifest(
        &template
            .manifest()
            .replace("components.main", "component")
            .replace(&sha256(&fixture_component()), "sha256:dev"),
    );
    let module = template.path.join("module.wasm");
    fs::write(&module, fixture_module()).unwrap();
    let output = template.path.join("built");
    build_component_package(
        &template.path,
        &BTreeMap::from([(String::new(), module)]),
        &output,
    )
    .unwrap();
    assert!(PluginPackage::load(output).unwrap().component("").is_some());
}

#[test]
fn manifest_rejects_configuration_projection() {
    let package = TestPackage::create();
    package.set_manifest(&package.manifest().replace(
        "[components.main]",
        "[components.main]\nconfig_pointer = \"/settings\"",
    ));
    assert!(PluginPackage::load(&package.path).is_err());
}

#[test]
fn source_world_requires_the_ingress_export() {
    let fixture = TestPackage::create();
    let package = PluginPackage::load(&fixture.path).unwrap();
    let mut manifest = package.component("main").unwrap().manifest().clone();
    let mut resolve = Resolve::default();
    let (package_id, _) = resolve
        .push_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../wit"))
        .unwrap();
    let world = resolve.select_world(&[package_id], Some("source")).unwrap();
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    let source = ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    assert!(
        crate::component::validate_component("pluribus:plugin@1.0.0", &manifest, &source).is_err()
    );
    manifest.world = "pluribus:plugin/source@1.0.0".into();
    crate::component::validate_component("pluribus:plugin@1.0.0", &manifest, &source).unwrap();
    assert!(
        crate::component::validate_component(
            "pluribus:plugin@1.0.0",
            &manifest,
            &fixture_component()
        )
        .is_err()
    );
}
