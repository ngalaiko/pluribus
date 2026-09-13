# OpenRouter

Serves `model.requested` through the OpenRouter chat-completions API using a host-owned API key. One instance serves the model slugs its configuration lists.

Responses stream as server-sent events. The plugin owns record framing and appends one `model.stream` event per read, so a long completion is observable without a durable event per token.

Build and package:

```sh
cargo build --manifest-path plugins/openrouter/Cargo.toml --target wasm32-unknown-unknown --release
cargo run -p pluribus-plugin-package --bin pluribus-package -- \
  plugins/openrouter \
  target/plugins/openrouter \
  main=plugins/openrouter/target/wasm32-unknown-unknown/release/pluribus_plugin_openrouter.wasm
```

Configuration:

```json
{
  "credentials": {"api-key": "openrouter:personal"},
  "models": [
    "anthropic/claude-sonnet-4.5",
    { "id": "openai/gpt-5.1", "features": ["vision", "reasoning"] }
  ],
  "timeout_ms": 300000
}
```

Register the instance in `config.json` under `--data-dir`, then enroll the key. The flow performs no HTTP, so it needs no `enrollment_origins`; the injection origin must appear in the `net.http` grant:

```json
{
  "plugin_instances": {
    "openrouter": {
      "package": "file:///path/to/pluribus-v2/target/plugins/openrouter",
      "config": {
        "credentials": {"api-key": "openrouter:personal"},
        "models": [
          "anthropic/claude-sonnet-4.5"
        ]
      },
      "components": {
        "main": {
          "http": {
            "origins": [
              "https://openrouter.ai"
            ],
            "methods": [
              "POST"
            ],
            "max_request_bytes": 16777216,
            "max_response_bytes": 16777216,
            "max_timeout_ms": 300000
          }
        }
      }
    }
  },
  "model_instance": "openrouter/main"
}
```

```sh
pluribus --data-dir ./data auth openrouter
```

`credential` is an opaque handle created from the plugin's `static-http@1` flow. The host stores the key and injects `authorization` on requests to `https://openrouter.ai`. Key bytes never enter the component.

A model entry is a slug or a descriptor. The descriptor's `features` narrows what the host advertises for that model.

## Requests

`max_output_tokens` becomes `max_tokens`, and `output_schema` becomes a strict `json_schema` response format. Tool names are rewritten to the provider's identifier alphabet and mapped back on the way out.

`provider_options` is copied by allowlist: `temperature`, `top_p`, `top_k`, `frequency_penalty`, `presence_penalty`, `repetition_penalty`, `seed`, `stop`, `reasoning`, `provider`, `transforms`, `models`, `route`. Anything else is dropped rather than forwarded.

## Limits

Chat completions are stateless, so this provider returns no continuation and rejects a request that carries one. Audio input and output are not implemented. Prompt caching is whatever the routed upstream does; the provider does not promise it.

A `402` error chunk maps to `resource-exhausted`, `429` to `unavailable`, and `400`, `404`, or `422` to `invalid-argument`. Host response-byte and time limits apply on top of `timeout_ms`.
