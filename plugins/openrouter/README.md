# OpenRouter

Serves `model.requested` through the OpenRouter chat-completions API using an enrolled API key. One instance serves the model slugs its configuration lists.

Responses stream as server-sent events. The plugin owns record framing and appends one `model.stream` event per read, so a long completion is observable without a durable event per token.

See [development](../../docs/development.md) to build packages.

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

Register the instance in `config.json` under `--data-dir`, then enroll the key. The request origin must appear in the `net.http` grant:

```json
{
  "plugin_instances": {
    "openrouter": {
      "package": "file:///path/to/pluribus/target/plugins/openrouter",
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
pluribus --data-dir ./data --config-dir ./data plugins auth openrouter
```

`credentials.api-key` is an opaque handle. The host seals the enrolled key under it; the component reads it back with `credentials.get` and sets `authorization` itself.

A model entry is a slug or a descriptor. The descriptor's `features` narrows what the host advertises for that model.

## Requests

`max_output_tokens` becomes `max_tokens`, and `output_schema` becomes a strict `json_schema` response format. Tool names are rewritten to the provider's identifier alphabet and mapped back on the way out.

`provider_options` is copied by allowlist: `temperature`, `top_p`, `top_k`, `frequency_penalty`, `presence_penalty`, `repetition_penalty`, `seed`, `stop`, `reasoning`, `provider`, `transforms`, `models`, `route`. Anything else is dropped rather than forwarded.

## Limits

Chat completions are stateless, so this provider returns no continuation and rejects a request that carries one. Audio input and output are not implemented. Prompt caching is whatever the routed upstream does; the provider does not promise it.

A `402` error chunk maps to `resource-exhausted`, `429` to `unavailable`, and `400`, `404`, or `422` to `invalid-argument`. Host response-byte and time limits apply on top of `timeout_ms`.
