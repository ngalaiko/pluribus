# OpenAI Codex subscription

Calls the ChatGPT Codex Responses backend using a host-owned OAuth credential. It declares device enrollment through `credential-provider`; it does not invoke or embed the Codex CLI.

Responses stream through host-managed SSE. Each model delta is audited before the next frame is read.

Build and package:

```sh
cargo build --manifest-path plugins/openai-codex/Cargo.toml --target wasm32-unknown-unknown --release
cargo run -p pluribus-plugin-package --bin pluribus-package -- \
  plugins/openai-codex \
  target/plugins/openai-codex \
  main=plugins/openai-codex/target/wasm32-unknown-unknown/release/pluribus_plugin_openai_codex.wasm
```

Configuration:

```json
{
  "credentials": {"subscription": "openai-codex:personal"},
  "models": ["gpt-5.6-luna"],
  "timeout_ms": 300000
}
```

`credential` is an opaque handle created from the plugin's declarative flow. The host executes enrollment, stores and refreshes tokens, and injects requests. Token bytes never enter the component.

The Codex backend does not accept `max_output_tokens`; this provider omits that optional hint. Host response-byte and time limits still apply.

The v2 subscription recipe binds `chatgpt-account-id`, uses the earliest response/JWT expiry, and permits one retry only for a rejected `POST /backend-api/codex/responses`. Other requests and established SSE streams do not replay.

For an existing credential, run `pluribus auth <instance> --adopt-recipe` to validate and attach the installed recipe without rotating tokens. If its identity or scope cannot be verified, run normal `pluribus auth <instance>`.
