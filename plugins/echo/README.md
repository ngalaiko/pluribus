# Echo plugin

Reference `capability-plugin` implementation. `system.echo` returns its validated arguments.

Build and package:

```sh
cargo build --manifest-path plugins/echo/Cargo.toml --target wasm32-unknown-unknown --release
cargo run -p pluribus-plugin-package --bin pluribus-package -- \
  plugins/echo \
  plugins/echo/target/wasm32-unknown-unknown/release/pluribus_plugin_echo.wasm \
  target/plugins/echo
```
