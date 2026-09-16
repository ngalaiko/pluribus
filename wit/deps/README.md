# WASI definitions

Unmodified WASI 0.3.0 WIT from Wasmtime 48.0.1:

- `http.wit`: `wasmtime-wasi-http/src/p3/wit/deps`
- Other packages: `wasmtime-wasi/src/p3/wit/deps`

The HTTP package references CLI worlds, whose definitions reference filesystem
and socket packages. WIT resolution requires those transitive definitions.
Vendoring a package does not import it or grant access.

The Pluribus world imports only HTTP types/client, clocks, and secure randomness.
It imports no filesystem, CLI, or general network sockets.

Sources: https://github.com/bytecodealliance/wasmtime/tree/v48.0.1/crates/wasi
and https://github.com/bytecodealliance/wasmtime/tree/v48.0.1/crates/wasi-http.
See https://wasi.dev/security and https://wasi.dev/releases/wasi-p3.
