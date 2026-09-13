# Echo plugin

`system.echo` returns its arguments. Assigned `http.request.received` events
produce HTTP 200 with the exact request body as `application/octet-stream`.
HTTP replies do not increment the capability invocation counter.

Build and package:

```sh
cargo build --release --target wasm32-unknown-unknown --lib -p pluribus-plugin-echo
cargo run -p pluribus-plugin-package --bin pluribus-package -- \
  plugins/echo target/plugins/echo \
  =target/wasm32-unknown-unknown/release/pluribus_plugin_echo.wasm
```

Route `echo:GET,POST:/*:echo` through the HTTP listener and install the echo
component as `echo`. The end-to-end fixture drives the real agent event loop:

```sh
cargo test -p pluribus-plugin-http --test echo_http
```
