use crate::path::{read_package_file, validate_package_root, validate_relative_path};
use crate::{Manifest, PackageError, PluginPackage, sha256, validate_manifest_schema};
use serde_json::Value;
use std::fs;
use std::path::{Component, Path};
use wit_component::ComponentEncoder;

const MAX_TEMPLATE_BYTES: u64 = 1024 * 1024;
const MAX_MODULE_BYTES: u64 = 64 * 1024 * 1024;

/// Builds every named component from core WebAssembly modules.
/// # Errors
/// Rejects missing bindings, unsafe references, or invalid components and schemas.
pub fn build_component_package(
    template_root: impl AsRef<Path>,
    modules: &std::collections::BTreeMap<String, std::path::PathBuf>,
    output_root: impl AsRef<Path>,
) -> Result<std::collections::BTreeMap<String, String>, PackageError> {
    let template_root = template_root.as_ref();
    let output_root = output_root.as_ref();
    validate_package_root(template_root)?;
    let manifest_bytes =
        read_package_file(template_root, Path::new("plugin.toml"), MAX_TEMPLATE_BYTES)?;
    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .map_err(|error| PackageError::new(format!("plugin.toml is not UTF-8: {error}")))?;
    let mut manifest_value: toml::Value = toml::from_str(manifest_text)
        .map_err(|error| PackageError::new(format!("invalid plugin.toml: {error}")))?;
    let manifest_json: Value = serde_json::to_value(&manifest_value)
        .map_err(|error| PackageError::new(format!("cannot normalize manifest: {error}")))?;
    validate_manifest_schema(&manifest_json)?;
    let manifest: Manifest = toml::from_str(manifest_text)
        .map_err(|error| PackageError::new(format!("invalid plugin.toml: {error}")))?;
    crate::validate_references(&manifest)?;
    if !manifest.components.keys().eq(modules.keys()) {
        return Err(PackageError::new(
            "module bindings must match all component names exactly",
        ));
    }
    let mut referenced = std::collections::BTreeSet::from([manifest.config_schema.clone()]);
    let mut files = std::collections::BTreeMap::new();
    let mut digests = std::collections::BTreeMap::new();
    crate::load_schema(template_root, &manifest.config_schema)?;
    for (name, declaration) in &manifest.components {
        if declaration.digest != "sha256:dev" {
            return Err(PackageError::new(
                "package template digests must be sha256:dev",
            ));
        }
        validate_relative_path(Path::new(&declaration.component))?;
        crate::load_schema(template_root, &declaration.config_schema)?;
        referenced.insert(declaration.config_schema.clone());
        for provided in &declaration.provides {
            for path in [&provided.arguments_schema, &provided.result_schema]
                .into_iter()
                .chain(provided.constraints_schema.iter())
            {
                crate::load_schema(template_root, path)?;
                referenced.insert(path.clone());
            }
        }
        let module = read_module(&modules[name])?;
        let component = ComponentEncoder::default()
            .module(&module)
            .map_err(|error| PackageError::new(format!("invalid core module {name}: {error}")))?
            .validate(true)
            .encode()
            .map_err(|error| PackageError::new(format!("cannot encode {name}: {error:#}")))?;
        crate::component::validate_component(&manifest.abi, declaration, &component)?;
        let digest = sha256(&component);
        manifest_value["components"][name]["digest"] = toml::Value::String(digest.clone());
        digests.insert(name.clone(), digest);
        files.insert(declaration.component.clone(), component);
    }
    for credential in &manifest.credentials {
        crate::load_schema(template_root, &credential.input_schema)?;
        referenced.insert(credential.input_schema.clone());
        let bytes = read_package_file(
            template_root,
            Path::new(&credential.flow),
            MAX_TEMPLATE_BYTES,
        )?;
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| PackageError::new(format!("invalid credential flow: {error}")))?;
        referenced.insert(credential.flow.clone());
    }
    for relative in referenced {
        if relative == "plugin.toml" || files.contains_key(&relative) {
            return Err(PackageError::new("conflicting package file references"));
        }
        let bytes = read_package_file(template_root, Path::new(&relative), MAX_TEMPLATE_BYTES)?;
        files.insert(relative, bytes);
    }
    let production_manifest = toml::to_string_pretty(&manifest_value)
        .map_err(|error| PackageError::new(format!("cannot encode manifest: {error}")))?;
    prepare_output_root(output_root)?;
    for (relative, bytes) in &files {
        write_output_file(output_root, Path::new(relative), bytes)?;
    }
    write_output_file(
        output_root,
        Path::new("plugin.toml"),
        production_manifest.as_bytes(),
    )?;
    PluginPackage::load(output_root)?;
    Ok(digests)
}

fn read_module(path: &Path) -> Result<Vec<u8>, PackageError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PackageError::new(format!("cannot inspect core module: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PackageError::new(
            "core module must be a regular file, not a symlink",
        ));
    }
    if metadata.len() > MAX_MODULE_BYTES {
        return Err(PackageError::new(format!(
            "core module exceeds {MAX_MODULE_BYTES} bytes"
        )));
    }
    fs::read(path).map_err(|error| PackageError::new(format!("cannot read core module: {error}")))
}

fn prepare_output_root(root: &Path) -> Result<(), PackageError> {
    if root.exists() {
        validate_package_root(root)
    } else {
        fs::create_dir_all(root)
            .map_err(|error| PackageError::new(format!("cannot create output directory: {error}")))
    }
}

fn write_output_file(root: &Path, relative: &Path, bytes: &[u8]) -> Result<(), PackageError> {
    validate_relative_path(relative)?;
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for part in parent.components() {
            let Component::Normal(segment) = part else {
                return Err(PackageError::new("unsafe output path"));
            };
            current.push(segment);
            if current.exists() {
                let metadata = fs::symlink_metadata(&current).map_err(|error| {
                    PackageError::new(format!("cannot inspect output path: {error}"))
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(PackageError::new(
                        "output path contains a symlink or non-directory",
                    ));
                }
            } else {
                fs::create_dir(&current).map_err(|error| {
                    PackageError::new(format!("cannot create output path: {error}"))
                })?;
            }
        }
    }
    let destination = root.join(relative);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(PackageError::new("output file is a symlink or non-file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(PackageError::new(format!(
                "cannot inspect output file: {error}"
            )));
        }
    }
    fs::write(destination, bytes)
        .map_err(|error| PackageError::new(format!("cannot write package output: {error}")))
}
