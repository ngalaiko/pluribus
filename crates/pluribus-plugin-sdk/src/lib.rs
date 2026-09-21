#![doc = include_str!("../README.md")]
#![allow(unsafe_op_in_unsafe_fn)]

wit_bindgen::generate!({
    generate_all,
    path: "../../wit",
    world: "plugin",
    pub_export_macro: true,
    default_bindings_module: "pluribus_plugin_sdk",
    generate_unused_types: true,
});

pub mod http;
mod run;
pub mod socket;

pub use run::serve;
