# OpenAI Codex subscription

Calls the ChatGPT Codex Responses backend with a ChatGPT subscription token pair the plugin owns. It does not invoke or embed the Codex CLI.

Responses stream through host-managed SSE. Each model delta is audited before the next frame is read.

Build and package:

```sh
cargo build -p pluribus-plugin-openai-codex --target wasm32-unknown-unknown --release
cargo run -p pluribus-plugin-package --bin pluribus-package -- \
  plugins/openai-codex \
  target/plugins/openai-codex \
  main=target/wasm32-unknown-unknown/release/pluribus_plugin_openai_codex.wasm
```

Configuration:

```json
{
  "credentials": {"subscription": "openai-codex:personal"},
  "models": ["gpt-5.6-luna"],
  "timeout_ms": 300000
}
```

Start device login:

```sh
pluribus --data-dir ./data auth openai-codex
```

Open the displayed URL and enter the displayed code. Enable device-code login
in [ChatGPT security settings](https://learn.chatgpt.com/docs/auth) if required.
The node must be running to process enrollment and polling timers.

The plugin starts, polls, and completes device authorization. Pending device
state and tokens remain in the sealed credential record; only the verification
URL, user code, and scheduling metadata enter events. Pending enrollment resumes
after restart. Re-enrollment replaces the pending attempt.

`credentials.subscription` is an opaque handle. The component reads the sealed
pair with `credentials.get`, sets `authorization` and the
`chatgpt-account-id` claim it decodes from the access token, and, when
`https://chatgpt.com` rejects the token, exchanges the refresh token at
`https://auth.openai.com/oauth/token` and seals the replacement with
`credentials.compare-and-swap`. One rejection is retried; an established SSE
stream is not.

The grant must cover both origins:

```json
{"main": {"http": {"origins": ["https://chatgpt.com", "https://auth.openai.com"], "methods": ["POST"]}}}
```

The Codex backend does not accept `max_output_tokens`; this provider omits that optional hint. Host response-byte and time limits still apply.
