//! Generated host bindings for the plugin world.
//!
//! One world, so one set of host-import traits. Each additional world
//! generated a distinct trait for the same interface and forced the host to
//! implement it again.

#![allow(unsafe_code, unused_mut, unused_variables)]

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "plugin",
    with: { "wasi:http/types": wasmtime_wasi_http::p3::bindings::http::types },
    imports: {
        "wasi:random/random.get-random-bytes": async | trappable,
        "wasi:random/random.get-random-u64": async | trappable,
        default: async
    },
    exports: {

        default: async,
    },
});
