//! Loading and validation for unpacked Pluribus plugin packages.

mod component;
mod manifest;
mod package_builder;
mod path;

pub use manifest::{
    ComponentManifest, CredentialDeclaration, Idempotency, Manifest, ModelProvider,
    ProvidedCapability, RequestedCapability,
};
pub use package_builder::build_component_package;

use crate::component::validate_component;
use crate::path::{read_package_file, validate_package_root};
use jsonschema::Validator;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const MANIFEST_SCHEMA: &str = include_str!("../../../schemas/plugin-manifest.schema.json");
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_CONFIG_SCHEMA_BYTES: u64 = 1024 * 1024;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;

/// A validated package ready for runtime compilation.
pub struct PluginPackage {
    root: PathBuf,
    manifest: Manifest,
    components: BTreeMap<String, PluginComponent>,
    config_validator: Validator,
}

pub struct PluginComponent {
    name: String,
    root: PathBuf,
    manifest: ComponentManifest,
    component: Vec<u8>,
    config_validator: Validator,
}

impl PluginPackage {
    /// Loads and validates an unpacked plugin directory.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, malformed metadata, digest mismatch,
    /// invalid component types, or manifest/component disagreement.
    pub fn load(root: impl AsRef<Path>) -> Result<Self, PackageError> {
        let root = root.as_ref();
        validate_package_root(root)?;

        let manifest_bytes = read_package_file(root, Path::new("plugin.toml"), MAX_MANIFEST_BYTES)?;
        let manifest_text = std::str::from_utf8(&manifest_bytes)
            .map_err(|error| PackageError::new(format!("plugin.toml is not UTF-8: {error}")))?;
        let manifest_value: toml::Value = toml::from_str(manifest_text)
            .map_err(|error| PackageError::new(format!("invalid plugin.toml: {error}")))?;
        let manifest_json = serde_json::to_value(manifest_value)
            .map_err(|error| PackageError::new(format!("cannot normalize plugin.toml: {error}")))?;
        validate_manifest_schema(&manifest_json)?;
        let manifest: Manifest = toml::from_str(manifest_text)
            .map_err(|error| PackageError::new(format!("invalid plugin.toml: {error}")))?;

        validate_references(&manifest)?;
        let config_validator = load_schema(root, &manifest.config_schema)?;
        let mut components = BTreeMap::new();
        for (name, declaration) in &manifest.components {
            let component =
                read_package_file(root, Path::new(&declaration.component), MAX_COMPONENT_BYTES)?;
            verify_digest(&declaration.digest, &component)?;
            validate_component(&manifest.abi, declaration, &component)?;
            let config_validator = load_schema(root, &declaration.config_schema)?;
            for capability in &declaration.provides {
                load_schema(root, &capability.arguments_schema)?;
                load_schema(root, &capability.result_schema)?;
                if let Some(path) = &capability.constraints_schema {
                    load_schema(root, path)?;
                }
            }
            components.insert(
                name.clone(),
                PluginComponent {
                    name: name.clone(),
                    root: root.to_path_buf(),
                    manifest: declaration.clone(),
                    component,
                    config_validator,
                },
            );
        }
        for credential in &manifest.credentials {
            load_schema(root, &credential.input_schema)?;
            let bytes =
                read_package_file(root, Path::new(&credential.flow), MAX_CONFIG_SCHEMA_BYTES)?;
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|error| PackageError::new(format!("invalid credential flow: {error}")))?;
        }

        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            components,
            config_validator,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    #[must_use]
    pub fn components(&self) -> &BTreeMap<String, PluginComponent> {
        &self.components
    }

    #[must_use]
    pub fn component(&self, name: &str) -> Option<&PluginComponent> {
        self.components.get(name)
    }

    /// Validates one instance configuration.
    ///
    /// # Errors
    ///
    /// Returns the first schema violation.
    pub fn validate_config(&self, config: &Value) -> Result<(), PackageError> {
        self.config_validator
            .validate(config)
            .map_err(|error| PackageError::new(format!("invalid plugin configuration: {error}")))?;
        for component in self.components.values() {
            component.validate_config(component.project_config(config)?)?;
        }
        Ok(())
    }
}

impl PluginComponent {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    #[must_use]
    pub fn manifest(&self) -> &ComponentManifest {
        &self.manifest
    }
    #[must_use]
    pub fn component(&self) -> &[u8] {
        &self.component
    }
    /// Projects configuration from the package configuration.
    /// # Errors
    /// Fails when the declared JSON pointer is absent.
    pub fn project_config<'a>(&self, config: &'a Value) -> Result<&'a Value, PackageError> {
        config
            .pointer(&self.manifest.config_pointer)
            .ok_or_else(|| {
                PackageError::new(format!("missing configuration for component {}", self.name))
            })
    }
    /// Validates projected component configuration.
    /// # Errors
    /// Returns a schema violation.
    pub fn validate_config(&self, config: &Value) -> Result<(), PackageError> {
        self.config_validator.validate(config).map_err(|error| {
            PackageError::new(format!(
                "invalid component {} configuration: {error}",
                self.name
            ))
        })
    }
}

fn load_schema(root: &Path, relative: &str) -> Result<Validator, PackageError> {
    let bytes = read_package_file(root, Path::new(relative), MAX_CONFIG_SCHEMA_BYTES)?;
    let schema: Value = serde_json::from_slice(&bytes)
        .map_err(|error| PackageError::new(format!("invalid schema {relative}: {error}")))?;
    jsonschema::validator_for(&schema)
        .map_err(|error| PackageError::new(format!("invalid schema {relative}: {error}")))
}

fn validate_references(manifest: &Manifest) -> Result<(), PackageError> {
    fn visit<'a>(
        name: &'a str,
        manifest: &'a Manifest,
        active: &mut BTreeSet<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> Result<(), PackageError> {
        if done.contains(name) {
            return Ok(());
        }
        if !active.insert(name) {
            return Err(PackageError::new("cyclic component requirements"));
        }
        for dependency in &manifest.components[name].requires {
            visit(dependency, manifest, active, done)?;
        }
        active.remove(name);
        done.insert(name);
        Ok(())
    }
    let mut capabilities = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for (name, component) in &manifest.components {
        if !paths.insert(&component.component) {
            return Err(PackageError::new("duplicate component path"));
        }
        for required in &component.requires {
            if required == name || !manifest.components.contains_key(required) {
                return Err(PackageError::new(format!(
                    "invalid component requirement {name}: {required}"
                )));
            }
        }
        for capability in &component.provides {
            if !capabilities.insert(&capability.capability) {
                return Err(PackageError::new(format!(
                    "duplicate capability provider: {}",
                    capability.capability
                )));
            }
        }
    }
    let mut done = BTreeSet::new();
    for name in manifest.components.keys() {
        visit(name, manifest, &mut BTreeSet::new(), &mut done)?;
    }
    let mut ids = BTreeSet::new();
    for credential in &manifest.credentials {
        if !ids.insert(&credential.id) {
            return Err(PackageError::new("duplicate credential declaration"));
        }
        for name in &credential.components {
            if !manifest.components.contains_key(name) {
                return Err(PackageError::new(format!(
                    "unknown credential component: {name}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_manifest_schema(value: &Value) -> Result<(), PackageError> {
    let schema: Value = serde_json::from_str(MANIFEST_SCHEMA)
        .map_err(|error| PackageError::new(format!("invalid built-in manifest schema: {error}")))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| PackageError::new(format!("invalid built-in manifest schema: {error}")))?;
    validator
        .validate(value)
        .map_err(|error| PackageError::new(format!("invalid plugin manifest: {error}")))
}

fn verify_digest(expected: &str, bytes: &[u8]) -> Result<(), PackageError> {
    if expected == "sha256:dev" {
        return Err(PackageError::new(
            "development component digest cannot be activated",
        ));
    }
    let actual = sha256(bytes);
    if expected == actual {
        Ok(())
    } else {
        Err(PackageError::new(format!(
            "component digest mismatch: expected {expected}, got {actual}"
        )))
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{hex}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageError(String);

impl PackageError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for PackageError {}

#[cfg(test)]
mod tests;
