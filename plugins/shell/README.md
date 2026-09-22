# Shell

`shell.execute` runs `/bin/sh -c` through an executor process under a separate workspace account. The component reaches it over its granted [byte stream](../../docs/plugins/stream.md) and explicitly granted credential exports.

The plugin package contains both the Wasm component and the executor binary in a single `Cargo.toml`.

Arguments: `command` and optional `timeout_ms` (default 30000, maximum 300000). Output: `stdout`, `stderr`, `exit_code`, and `truncated`. Output is capped at 1 MiB combined before UTF-8 decoding. Commands start in the configured workspace with a cleared environment, fixed PATH, and workspace HOME. Files persist between calls; shell state does not.

Commands are non-idempotent. The transport makes one attempt and does not retry. Disconnect, timeout, and emergency stop terminate the command's process group. Deliberately detached processes escape that group. Containing them is the operator's choice of supervisor: a systemd control group, a launchd job, a container, or a jail. The executor makes no such guarantee on its own.

## Build

```sh
cargo build --release -p pluribus-cli
cargo build --locked --release --target wasm32-unknown-unknown --lib -p pluribus-plugin-shell
cargo build --locked --release -p pluribus-plugin-shell --bin pluribus-shell-executor
cargo build --locked --release -p pluribus-plugin-shell --bin pluribus-shell-cli
```

## Deployment

The executor runs on any host with a Unix peer-credential call: Linux via `SO_PEERCRED`, macOS and the BSDs via `getpeereid`. Hosts without one do not build.

Use distinct non-root accounts, for example `pluribus-personal` and `pluribus-workspace`. The workspace account must have no access to the runtime's files, process inspection, credentials, sudo privileges, or administrative sockets. Keep runtime state at mode 0700 and runtime binaries, plugin packages, and service definitions owned by the administrator. The executor has the workspace account's full authority.

Install `pluribus`, `pluribus-shell-executor`, and `pluribus-shell-cli` under `/usr/local/bin`, the shell package under `/opt/pluribus/plugins/shell`, and create `/srv/pluribus-workspace` owned by `pluribus-workspace`. Give the runtime account membership in the workspace group for socket access; do not give the workspace account membership in the runtime group.

The executor is a plain foreground process:

```sh
pluribus-shell-executor \
  --socket /run/pluribus-workspace/executor.sock \
  --workspace /srv/pluribus-workspace \
  --runtime-uid "$(id -u pluribus-personal)"
```

Run it under whatever supervisor the host provides. A systemd unit, for one; replace `1001` with `id -u pluribus-personal`:

```ini
[Unit]
Description=Pluribus workspace executor

[Service]
User=pluribus-workspace
Group=pluribus-workspace
RuntimeDirectory=pluribus-workspace
RuntimeDirectoryMode=0750
UMask=0077
ExecStartPre=/usr/bin/rm -f /run/pluribus-workspace/executor.sock
ExecStart=/usr/local/bin/pluribus-shell-executor --socket /run/pluribus-workspace/executor.sock --runtime-uid 1001 --workspace /srv/pluribus-workspace
NoNewPrivileges=true
KillMode=control-group
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Every flag above has a default for trying things out: the endpoint is
`shell-main.sock` in the agent's data directory (`$XDG_DATA_HOME/pluribus`),
commands run in the current directory, and the runtime is this account. That last default means
commands run with this account's authority, which the executor says at startup;
a deployment names another account with `--runtime-uid`, as above.

The socket is mode 0660. Both endpoints verify kernel-reported peer credentials; root and unexpected peers are rejected, and a peer sharing the runtime's account is allowed only where the grant lists that UID. Runtime startup also requires its data directory to be owned by the runtime account with no group or other permissions (mode 0700).

Merge into the runtime's `config.json`; replace `1002` with `id -u pluribus-workspace`:

```json
{
  "plugin_instances": {
    "shell-1": {
      "package": "file:///opt/pluribus/plugins/shell",
      "config": {},
      "components": {
        "main": {
          "stream": {
            "default": {
              "socket": "/run/pluribus-workspace/executor.sock",
              "peer_uids": [1002]
            }
          }
        }
      }
    }
  },
  "capability_instances": [
    "shell-1/main"
  ],
  "trusted_capabilities": {
    "shell-1/main": [
      "shell.execute"
    ]
  }
}
```

Preserve existing connector, model, and registry entries. For Telegram, add the operator's sender ID to `plugin_instances.telegram.config.trusted_senders`. Restart the runtime after configuration changes.

Run the primary as `pluribus-personal`, with its own private state directory at mode 0700 and a umask of 0077. Under systemd that is `StateDirectory=pluribus-personal`, `StateDirectoryMode=0700`, `UMask=0077`, `SupplementaryGroups=pluribus-workspace`, and `ExecStart=/usr/local/bin/pluribus --data-dir /var/lib/pluribus-personal run`. Keep it separate from the executor service. Configure host process isolation so workspace processes cannot inspect runtime processes.

Verify from a trusted Telegram account: ask the agent to run `id; pwd; printf hello`. Verify an unknown sender cannot execute it. Check runtime events for `policy.decision`, `capability.*`, and `stream.*`. The command and its output are audited at the capability boundary; the transport records only the endpoint and its ceilings.

As the runtime account:

```sh
pluribus --data-dir /var/lib/pluribus-personal stop
pluribus --data-dir /var/lib/pluribus-personal run --resume
```

`stop` persists a STOP marker, halts further cognition, and signals active components. Shell cancellation closes the socket and kills the process group. Existing model/connector requests may take their configured timeout to return. `run --resume` clears the marker; use it only after the previous runner exits.

## Core access from commands

Set the shell instance's `config`:

```json
{
  "credential_exports": {
    "GH_TOKEN": {
      "credential": "github:personal",
      "provider": "dev.pluribus.github",
      "export": "installation-token"
    }
  }
}
```

Commands request a granted export only when needed:

```sh
pluribus-shell-cli secret GH_TOKEN | gh auth login --with-token
```

The helper is a separate installed `pluribus-shell-cli` binary. Its directory is
added to the command PATH. The shell component resolves the binding through
core for each request; values never enter the command environment or event
log. Only names in `credential_exports` are accepted, and missing or expiring
exports fail the helper call. Keep secrets out of command output and logs.

The RLM runtime automatically forwards ready attachment references from the
current input to `shell.execute`; the model does not need to supply metadata.
The helper resolves a digest only among those references visible in the
current delivery and streams the bytes to stdout in chunks:

```sh
pluribus-shell-cli attachment SHA256_DIGEST | pdftotext - -
```

Pass only the attachment's lowercase 64-character SHA-256 digest. Core resolves it against the
current delivery's visible blobs; hidden blobs cannot be read. Redirect the
output to a workspace file to use it with local tools.

Upload a processed or generated file with `pluribus-shell-cli upload PATH
[MEDIA_TYPE]`. It prints a JSON blob reference on stdout. Parse that JSON and
pass the reference as `blob` to `telegram.send-media`, for example with
`kind: "photo"` or `kind: "document"`. A helper upload remains available only
when its returned reference is included in a later capability request.

The default shell stream grant is 16 MiB shared by both directions. Core
messages encode byte chunks as JSON arrays, so the practical file ceiling is
lower (roughly 4 MiB, less framing overhead). A larger grant raises that
ceiling. Shell command output still has a separate 1 MiB cap.

Bindings contain references only. The executor's `--path` controls where tools
such as `gh` are found; its own directory is added automatically.
