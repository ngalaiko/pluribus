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
fn one_world_with_run_and_event_delivery() {
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
        imports, 13,
        "Pluribus contracts plus WASI HTTP, clocks, and randomness"
    );
}

#[test]
fn socket_exposes_only_connect() {
    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve
        .push_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../wit"))
        .unwrap();
    let socket = resolve.packages[package].interfaces["socket"];
    let functions: Vec<_> = resolve.interfaces[socket].functions.values().collect();
    assert_eq!(functions.len(), 1, "socket exposes one function");
    let connect = functions[0];
    assert_eq!(connect.item_name(), "connect");
    assert!(matches!(
        connect.kind,
        wit_parser::FunctionKind::AsyncFreestanding
    ));
    assert_eq!(
        connect
            .params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["endpoint", "outgoing"]
    );
}

#[test]
fn run_belongs_to_lifecycle() {
    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve
        .push_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../wit"))
        .unwrap();
    let interface = resolve.packages[package].interfaces["lifecycle"];
    let functions: Vec<_> = resolve.interfaces[interface]
        .functions
        .values()
        .map(wit_parser::Function::item_name)
        .collect();
    assert!(functions.contains(&"run"));
    assert!(matches!(
        resolve.interfaces[interface]
            .functions
            .values()
            .find(|f| f.item_name() == "run")
            .unwrap()
            .kind,
        wit_parser::FunctionKind::AsyncFreestanding
    ));
    assert!(!functions.contains(&"init"));
    assert_eq!(functions.len(), 3);
    let run = resolve.interfaces[interface]
        .functions
        .values()
        .find(|f| f.item_name() == "run")
        .unwrap();
    assert_eq!(
        run.params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["context", "config"]
    );
    let runtime = resolve.packages[package].interfaces["runtime"];
    assert!(
        resolve.interfaces[runtime]
            .functions
            .values()
            .any(|f| f.item_name() == "ready"
                && matches!(f.kind, wit_parser::FunctionKind::AsyncFreestanding))
    );
    assert!(
        !resolve.interfaces[runtime]
            .functions
            .values()
            .any(|f| f.item_name() == "enable")
    );
}

#[test]
fn wasi_imports_do_not_grant_filesystem_or_ambient_network() {
    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve
        .push_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../wit"))
        .unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let imports = resolve.worlds[world]
        .imports
        .values()
        .filter_map(|item| match item {
            wit_parser::WorldItem::Interface { id, .. } => resolve.id_of(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    for expected in [
        "wasi:http/client@0.3.0",
        "wasi:http/types@0.3.0",
        "wasi:clocks/monotonic-clock@0.3.0",
        "wasi:clocks/system-clock@0.3.0",
        "wasi:random/random@0.3.0",
    ] {
        assert!(
            imports.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
    for forbidden in ["wasi:filesystem/", "wasi:sockets/", "wasi:cli/"] {
        assert!(
            !imports.iter().any(|name| name.starts_with(forbidden)),
            "unexpected {forbidden}"
        );
    }
}
