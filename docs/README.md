# Documentation

Start with [architecture](architecture.md) for the system model.

## Run an agent

- [Installation](installation.md): distribution, packages, upgrades, and cache.
- [Instances](plugin-instances.md): configuration, component access, and grants.
- [Operations](operations.md): paths, shutdown, backup, and recovery.
- [Plugins](../plugins/README.md): plugin setup and limits.

## Develop

- [Development](development.md): builds, tests, and local fixtures.
- [Host crates](../crates/README.md): implementation ownership.
- [Runtime](runtime.md): async execution and cancellation boundaries.
- [Events and authority](events-and-authority.md): shared event and permission model.
- [Plugin specification](plugins/README.md): public host/guest contract.
- [Logging](logging.md): process diagnostics.

## Maintain these docs

Keep shared behavior here and component details beside their implementation.
Give each fact one owner; summarize and link elsewhere. READMEs explain purpose,
boundaries, setup or development, and limits. Add nested READMEs only for distinct
components.

Keep plugin docs independent. Describe other providers through shared capability
and event contracts. Plugin-specific tools, behavior, examples, and evaluations
belong with their owning plugin; cross-plugin setup belongs in shared guides.

Document current behavior. Remove completed plans and obsolete claims. Update the
owning page with behavior changes and check relative links and examples. WIT,
schemas, CLI help, and Rust API docs own exact signatures and fields; prose
explains their meaning and interactions.
