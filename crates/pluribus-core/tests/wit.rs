use std::path::PathBuf;

#[test]
fn plugin_abi_resolves() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../wit");
    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve
        .push_dir(path)
        .unwrap_or_else(|error| panic!("plugin WIT must resolve: {error:#?}"));

    resolve
        .select_world(&[package], Some("plugin"))
        .unwrap_or_else(|error| panic!("world plugin must resolve: {error}"));
}

#[test]
fn one_world_and_one_export() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../wit");
    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve.push_dir(path).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();

    let worlds = resolve.packages[package].worlds.len();
    let exports = resolve.worlds[world].exports.len();
    let imports = resolve.worlds[world].imports.len();

    assert_eq!(worlds, 1, "the ABI defines exactly one world");
    assert_eq!(exports, 1, "a plugin exports only lifecycle");
    assert_eq!(
        imports, 8,
        "events, state, blobs, reader, writer, http, socket, and types as a type-only import"
    );
}
