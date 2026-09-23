# Plugins

Plugins provide behavior through events and capabilities. Packages may contain
several isolated Wasm components and native helpers. Installation and grants are
configured through [package instances](../docs/plugin-instances.md).

| Package | Role | Components or helpers |
| --- | --- | --- |
| [CLI](cli/README.md) | Terminal input and replies | Wasm connector; native bridge |
| [Telegram](telegram/README.md) | Messages and media | Separate receive/send components |
| [Email](email/README.md) | IMAP observations and SMTP replies | Wasm protocol handling over host TLS streams |
| [HTTP](http/README.md) | Inbound HTTP transport | Wasm adapter; native listener |
| [GitHub](github/README.md) | App credentials and webhook observations | Uses HTTP; exports credentials to shell |
| [OpenRouter](openrouter/README.md) | Model provider | Chat-completions API adapter |
| [OpenAI Codex](openai-codex/README.md) | Model provider | Subscription enrollment and Responses adapter |
| [RLM](rlm/README.md) | Reasoning and durable jobs | Cognition and JavaScript REPL components |
| [Shell](shell/README.md) | Command execution and file transfer | Wasm adapter; native executor and CLI |
| [Memory](memory/README.md) | Explicit retention and lexical retrieval | Wasm capability provider |
| [Echo](echo/README.md) | Reference plugin and test provider | Echo capability and HTTP responses |

`init --example` combines CLI, shell, OpenRouter, and RLM. Connectors emit
observations; RLM requests models and capabilities; the host authorizes and routes
those requests. Model selection belongs to agent configuration. Reply capabilities
belong to connectors. See [architecture](../docs/architecture.md).

Package READMEs own setup and limitations. The [plugin specification](../docs/plugins/README.md)
owns shared contracts; the [SDK](../crates/pluribus-plugin-sdk/README.md) and
[echo example](echo/README.md) are the Rust authoring entry points.

[Scheduler](scheduler/README.md) owns timers, one-time schedules, and cron observations.
