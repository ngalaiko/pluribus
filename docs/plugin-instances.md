# Package and component instances

`config.json` under `--data-dir` registers package instances. Each package declares named components; each component runs independently with the stable ID `package-instance/component-name`.

Telegram contains `receive` and `send`. RLM contains `cognition` and `js`. Each component has separate linear memory, state namespace, delivery cursor, cancellation handle, worker, imports, and grants. A blocked Telegram poll cannot occupy the sender's worker.

```json
{
  "plugin_instances": {
    "telegram-1": {
      "package": "file:///opt/plugins/telegram",
      "aliases": ["telegram"],
      "config": {"credential_handle": "telegram:personal", "poll_timeout_seconds": 30},
      "components": {
        "receive": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}},
        "send": {"http": {"origins": ["https://api.telegram.org"], "methods": ["POST"]}}
      }
    }
  }
}
```

This fragment needs a model package and `model_instance` to run. Packages use absolute `file://` directory URLs or pinned `file://` or `https://` archive references. Package configuration must satisfy its outer schema and each component's projected schema. The manifest's required `config_pointer` selects the component configuration; an empty pointer selects the full object. `components` access settings must match the package's declared component names.

## Configuration and authority

| Field | Meaning |
| --- | --- |
| `package` | `file://` directory URL or archive object with `url` and `sha256`. |
| `config` | Package configuration; credentials are opaque handles, never secrets. |
| `aliases` | Enrollment command aliases for the package instance. |
| `components.<name>.http` | Component HTTP origins, methods, request/response limits, and timeout. |
| `components.<name>.stream` | Component Unix socket endpoint and transfer limits. |
| `components.<name>.limits` | Component memory and lifecycle-call timeout. |
| `enrollment_origins` | Package OAuth enrollment and refresh origins. |

HTTP requires HTTPS, excludes private networks and redirects, and must match that component's declared requests. Runtime access does not follow from a manifest declaration alone. HTTP grant principals must match their component instance. Stream access additionally requires the endpoint's account to be listed in `peer_uids`; see [stream transport](plugins/stream.md).

Credential declarations belong to the package and name their authorized components. Enrollment grants the configured handle only to those component principals. A shared Telegram handle can serve its declared receiver and sender; unrelated components receive no access. Different package instances use distinct handles. Renaming a package instance changes its component principals, state namespaces, and cursors.

`model_instance` and entries in `capability_instances` select explicit `package-instance/component-name` IDs. A connector needs no such field: a component that emits `observation.received` is one provider's senses, and the component providing `<provider>.reply` answers it. `trusted_capabilities` and `trusted_constraints` use the same component IDs. Capability descriptors come from the selected component. Registration grants no authority by itself.

```json
{
  "capability_instances": ["echo-1/main"],
  "trusted_capabilities": {"echo-1/main": ["system.echo"]}
}
```

Inbound Telegram observations must originate from the selected receiver. Their origin retains that receiver's identity. Conversation-scoped reply grants name the selected sender and retain the original chat/thread restriction. Unknown senders receive those restricted reply grants; additional configured grants require a trusted sender.

## Installation boundary

The agent validates the entire package, all projected configurations, component names, subscription conflicts, and host grants, then compiles and instantiates every component before calling any `init`. A failed preflight starts nothing and registers nothing.

After preflight, each component initializes and rebuilds its projections. Components become active only after every lifecycle call succeeds. An initialization failure leaves no active registrations from that package; initialization events and state already committed by earlier components are not rolled back.

Each component handles deliveries serially. Different components run on separate workers. Dependencies declared in `requires` identify components within the package; they do not merge execution or grant domains.

## Discovery and enrollment

```sh
pluribus install ./packages/telegram
pluribus auth telegram
```

Discovery and enrollment read package descriptors without executing guest code. `install` reads a package's manifest and writes an instance with the access it requests. See [credentials](plugins/credentials.md).

