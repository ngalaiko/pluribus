#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[cfg(target_arch = "wasm32")]
mod component;
mod engine;
