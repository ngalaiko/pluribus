use crate::{ComponentManifest, PackageError};
use std::collections::BTreeSet;
use wit_parser::decoding::{DecodedWasm, decode};
use wit_parser::{Resolve, WorldItem};

const ABI_PACKAGE: &str = "pluribus:plugin@1.0.0";
const LIFECYCLE_EXPORT: &str = "pluribus:plugin/lifecycle@1.0.0";

pub(crate) fn validate_component(
    abi: &str,
    manifest: &ComponentManifest,
    bytes: &[u8],
) -> Result<(), PackageError> {
    let decoded = decode(bytes)
        .map_err(|error| PackageError::new(format!("invalid WebAssembly component: {error}")))?;
    let DecodedWasm::Component(resolve, world_id) = decoded else {
        return Err(PackageError::new(
            "plugin.wasm encodes a WIT package, not a component",
        ));
    };
    let world = &resolve.worlds[world_id];
    if abi != ABI_PACKAGE {
        return Err(PackageError::new(format!("unsupported plugin ABI: {abi}")));
    }

    let actual_imports = interface_ids(&resolve, world.imports.values(), "import", true)?;
    let declared_imports = manifest.imports.iter().cloned().collect::<BTreeSet<_>>();
    if actual_imports != declared_imports {
        return Err(set_mismatch("imports", &declared_imports, &actual_imports));
    }

    let actual_exports = interface_ids(&resolve, world.exports.values(), "export", false)?;
    let expected_exports = BTreeSet::from([String::from(LIFECYCLE_EXPORT)]);
    if actual_exports != expected_exports {
        return Err(set_mismatch("exports", &expected_exports, &actual_exports));
    }

    // `manifest.world` is not checked against the component: encoding renames
    // the component's own world to `root:component/root`, so the authored world
    // name does not survive. Import and export set equality above is the real
    // check.

    Ok(())
}

fn interface_ids<'a>(
    resolve: &Resolve,
    items: impl IntoIterator<Item = &'a WorldItem>,
    direction: &str,
    ignore_type_only: bool,
) -> Result<BTreeSet<String>, PackageError> {
    let mut ids = BTreeSet::new();
    for item in items {
        match item {
            WorldItem::Interface { id, .. } => {
                if ignore_type_only && resolve.interfaces[*id].functions.is_empty() {
                    continue;
                }
                let id = resolve.id_of(*id).ok_or_else(|| {
                    PackageError::new(format!("anonymous component interface {direction}"))
                })?;
                ids.insert(id);
            }
            WorldItem::Function(_) | WorldItem::Type { .. } => {
                return Err(PackageError::new(format!(
                    "top-level component {direction} is not allowed"
                )));
            }
        }
    }
    Ok(ids)
}

fn set_mismatch(
    label: &str,
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
) -> PackageError {
    let missing = expected.difference(actual).cloned().collect::<Vec<_>>();
    let unexpected = actual.difference(expected).cloned().collect::<Vec<_>>();
    PackageError::new(format!(
        "component {label} mismatch: missing {missing:?}, unexpected {unexpected:?}"
    ))
}
