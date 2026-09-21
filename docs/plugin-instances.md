# Package and component instances

`config.json` under `--data-dir` assembles an agent from package instances and operator overrides. Each package declares named components; each component runs independently with the stable ID `package-instance/component-name`.

Telegram contains `receive` and `send`. RLM contains `cognition` and `repl`. Each component has separate linear memory, state namespace, delivery cursor, cancellation handle, worker, imports, and grants. A blocked Telegram poll cannot occupy the sender's worker.

```json
{
  "plugin_instances": {
    "telegram-1": {
      "package": "file:///opt/plugins/telegram",
      "aliases": ["telegram"],
      "config": {"credentials": {"bot-token": "telegram:personal"}, "poll_timeout_seconds": 30},
      "components": {
        "receive": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}},
        "send": {"http": {"origins": ["https://api.telegram.org"], "methods": ["POST"]}}
      }
    }
  }
}
```

This fragment needs a model package and `model_instance` to run. Packages use absolute `file://` directory URLs or pinned `file://` or `https://` archive references. Package configuration must satisfy its outer schema and every component's schema. Each component receives the full instance configuration. `components` contains optional access overrides. Omitted components use manifest requests and runtime defaults; unknown component names are rejected. Single-component plugins can use `access` directly instead of a `components` map. An unnamed single-component plugin uses its instance ID directly. A bare instance selector also resolves a sole named component.

## Configuration and authority

| Field | Meaning |
| --- | --- |
| `package` | `file://` directory URL or archive object with `url` and `sha256`. |
| `config` | Package configuration; credentials are opaque handles, never secrets. |
| `aliases` | Enrollment command aliases for the package instance. |
| `components.<name>.http` | Component HTTP origins, methods, request/response limits, and timeout. |
| `components.<name>.stream` | Component endpoints by name, each a Unix socket or TLS destination with its own transfer limits. |
| `components.<name>.limits` | Optional memory and lifecycle-call timeout overrides; defaults: 32 MiB and 60 seconds. |

HTTP requires HTTPS, excludes private networks and redirects, and must match that component's declared requests. Runtime access does not follow from a manifest declaration alone. HTTP grant principals must match their component instance. Stream access additionally requires the endpoint's account to be listed in `peer_uids`; see [stream transport](plugins/stream.md).

Credential declarations belong to the package and name their authorized components. Enrollment grants the configured handle only to those component principals. A shared Telegram handle can serve its declared receiver and sender; unrelated components receive no access. Different package instances use distinct handles. Renaming a package instance changes its component principals, state namespaces, and cursors.

`model_instance` and entries in `capability_instances` select explicit `package-instance/component-name` IDs. A connector needs no such field: a component that emits `observation.received` is one provider's senses, and the component providing `<provider>.reply` answers it. `trusted_capabilities` and `trusted_constraints` use the same component IDs. Capability descriptors come from the selected component. Registration grants no authority by itself.

```json
{
  "capability_instances": ["echo-1/main"],
  "trusted_capabilities": {"echo-1/main": ["system.echo"]}
}
```

Inbound Telegram observations must originate from the selected receiver. Their origin retains that receiver's identity. Conversation-scoped reply grants name the selected sender and retain the original conversation restriction. Connectors control admission. Telegram admits only IDs in its `config.trusted_senders`; all other updates are ignored. Admitted observations receive the configured grants.

## Installation boundary

The agent validates the entire package, configuration against every component schema, component names, subscription conflicts, and host grants, then compiles and instantiates every component before starting any `run`. A failed preflight starts nothing and registers nothing.

After preflight, each component initializes and rebuilds its projections. Components become active only after every lifecycle call succeeds. An initialization failure leaves no active registrations from that package; initialization events and state already committed by earlier components are not rolled back.

Each component handles deliveries serially. Different components run on separate workers. Dependencies declared in `requires` identify components within the package; they do not merge execution or grant domains.

## Discovery and enrollment

```sh
pluribus install ./packages/telegram
pluribus auth telegram
```

Discovery and enrollment read package descriptors without executing guest code. `install` reads a package's manifest and writes an instance with the access it requests. See [credentials](plugins/credentials.md).


## Defaults and overrides

Installing a plugin accepts its declared HTTP and stream requirements. Startup
derives host access from `requested_capabilities`, then applies `components`
overrides. An explicit empty HTTP origins list disables network access. Credentials
use declared consumers and generated handle references; secret values come from
`pluribus auth`. Credential enrollment origins belong in the manifest and can be
overridden in config, including with an empty list.

Package `[defaults]` values merge recursively with instance `config`. Arrays and
scalar values replace defaults. Derived configuration and access are transient;
saving config preserves only operator overrides.

```json
{
  "plugin_instances": {
    "codex": {"package": "bundled:openai-codex"},
    "rlm": {"package": "bundled:rlm"},
    "telegram": {"package": "bundled:telegram"}
  }
}
```

Agent identity, model selection, and capability grants remain core settings.
Connectors control sender admission. Plugin
configuration does not supply a second RLM identity or connector list.
