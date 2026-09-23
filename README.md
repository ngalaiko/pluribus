# Pluribus

A plugin-based agent runtime. The host persists events, enforces authority, and
runs WebAssembly components. Plugins provide connectors, models, reasoning,
memory, and tools.

## Try it

```sh
nix profile install github:ngalaiko/pluribus
pluribus init --example
```

The example configures CLI input, shell execution, OpenRouter, and RLM cognition.
It enrolls a shared OpenRouter key with no paid budget for `openrouter/free`.
Use `pluribus plugins auth openrouter` to enroll your own key.

Start each command in a separate terminal:

```sh
pluribus-cli-bridge
pluribus-shell-executor
pluribus run
```

Type in the bridge terminal. The shell executor runs commands as your account,
in its working directory. For a deployment, configure a separate account and
explicit grants; see [shell deployment](plugins/shell/README.md#deployment).

Release archives support installation without Nix. See
[installation](docs/installation.md) for packages, upgrades, and offline use.

## Understand and extend

- [Architecture](docs/architecture.md): event flow, ownership, and trust boundaries.
- [Operations](docs/operations.md): paths, stopping, backup, and recovery.
- [Plugins](plugins/README.md): connectors, providers, and their configuration.
- [Host crates](crates/README.md): implementation map and crate responsibilities.
- [Development](docs/development.md): build and verification commands.
- [Write a plugin](docs/plugins/README.md): package, ABI, and lifecycle contracts.
- [Documentation](docs/README.md): shared guides and references.
