# Pluribus

## Install

```sh
nix profile install github:ngalaiko/pluribus
```

That carries the binary and every plugin package. Choose a subset, or add
plugins from other repositories:

```nix
pluribus.withPlugins (ps: [
  ps.memory
  ps.telegram
  inputs.my-plugin.packages.${system}.default
])
```

Each plugin also installs on its own:

```sh
nix profile install github:ngalaiko/pluribus#plugin-memory
```

A plugin package holds `share/pluribus/plugins/<name>`, plus `bin/` for a native
half: the shell plugin's executor, the cli plugin's bridge. Another repository
builds one with this flake's builder:

```nix
pluribus.lib.${system}.buildPluginPackage {
  name = "battery";
  src = ./.;                                  # plugin.toml, schemas, flows
  components.main = "${wasm}/battery.wasm";   # what the manifest declares
  binaries = [ ];                             # native halves, if any
}
```

Releases publish archives for machines without Nix: unpack one and put its
`bin` on PATH. Their binaries carry the release's package catalog, so a release
binary with nothing installed beside it fetches the packages `init --example`
configures by URL and digest.

## Run it locally

An agent you type at, thinking on OpenRouter's free tier, able to run shell
commands:

```sh
nix profile install github:ngalaiko/pluribus
pluribus init --example
```

`--example` configures the packages this build ships — `cli` to talk to,
`shell` to run commands, `openrouter` for cognition on `openrouter/free`, and
`rlm` to think with. It enrolls a shared OpenRouter key of its own, which has
no budget, so nothing is typed and no account is needed. Replace it with
`pluribus auth openrouter` for anything beyond trying this out.

Then start each of these, in its own terminal:

```sh
pluribus-cli-bridge
pluribus-shell-executor
pluribus run
```

They agree on where things live: the agent keeps its configuration, state and
endpoints in `$XDG_DATA_HOME/pluribus`, the executor runs commands in the
current directory, and both answer the account that started them. `--data-dir`
moves the agent; nothing depends on where you start it from. Type in the
bridge's terminal:

```
you: name three colors

agent: Red, green, and blue.
```

Everything runs as you, which is what makes this a two-minute trial rather than
a deployment: commands run with your authority, and the executor says so at
startup. A real installation runs the agent under its own account and names it
with `--runtime-uid`; see [plugins/shell](plugins/shell/README.md).

Add a plugin by hand with `pluribus install <package>`, naming a package
directory or an archive URL. An archive without `--sha256` is pinned to what
arrives, and every later fetch checks it. The instance it writes carries the
access the manifest asks for; credentials and model choices stay yours.

## Build

```sh
nix-build -A pluribus       # the binary and every plugin package
nix-build -A release        # release archives and the package catalog
nix-shell                   # the toolchain the workspace builds with
```

`pluribus-sync-plugins`, on the shell's PATH, writes the packages to
`target/plugins/<name>`, where the workspace tests read them:

```sh
nix-shell
pluribus-sync-plugins
cargo test --locked --workspace
```

`nix-build` and `nix-shell` read `flake.lock`, so they pin what the flake pins.
Use them in a working tree: `nix build .#` copies the whole tree, `target`
included.

`rust-toolchain.toml` pins the compiler for builds outside Nix; rustup installs
it on first use. `nix/` holds the build, packaging, and release derivations.

An installed directory contains `bin/` and `share/pluribus/plugins/`. Add its
`bin` directory to PATH. It can move without changing configuration; Rust is
needed only for source builds. Install into a new directory for each version.

An agent lives in `$XDG_DATA_HOME/pluribus`. Keep another one elsewhere with the
global flag, which every command accepts:

```sh
pluribus --data-dir /var/lib/pluribus init --example
```

Connectors control sender admission. Telegram admits only user IDs listed in
`plugin_instances.telegram.config.trusted_senders`; an empty or omitted list
ignores everyone. Admitted observations receive the configured capability grants.

`pluribus auth <instance>` stores or replaces the credential of any instance
whose manifest declares one, naming it by its configured ID or alias:

```sh
pluribus auth openrouter
pluribus auth mail-work
```

Secrets are sealed as scoped credentials; configuration carries only the handle.
Piped input is supported for automation.

Define packages, configurations, aliases, and grants in the [plugin-instance registry](docs/plugin-instances.md).

Run:

```sh
pluribus run
```

Observations, raw payloads, media descriptors, model deltas, capability calls,
and delivery outcomes persist under the data directory.

Receive [HTTP requests](plugins/http/README.md) and [GitHub observations](plugins/github/README.md), with App tokens available to the shell executor.

Observe [arriving mail over IMAP](plugins/email/README.md) and answer it over
SMTP. The host holds a TLS connection to each configured endpoint, speaking
the STARTTLS preamble where submission needs one; the plugin speaks IMAP and
SMTP, and waits on server push rather than polling.

Add [durable memory](plugins/memory/README.md), [configured tools](docs/plugin-instances.md) and [isolated shell execution](plugins/shell/README.md).

Stop cognition and cancel active shell commands with `pluribus stop`. Restart after stopping with `pluribus run --resume`. Use the same `--data-dir` for both commands.

[Persistent jobs, checkpoints, budgets, and recovery](docs/persistent-work.md). Operators coordinate concurrent runners and installations.

[Stopping, data layout, and verification](docs/operations.md).

Build scripts share `rust-toolchain.toml`, use locked dependencies, and permit
downloads on a fresh machine. Set `CARGO_NET_OFFLINE=true` for offline builds.
`PLURIBUS_PLUGIN_DIR` overrides the package root, the directory holding
`<name>/plugin.toml`.
Local package directories use absolute `file://` URLs. Update them when moving packages.
Archive references use `file://` or `https://` URLs with a SHA-256 hash.

[Release packaging and remote plugin installation](docs/release-design.md).

Package sources may also be `{ "url": "https://host/plugin.tar.gz", "sha256": "<archive hash>" }`.
`file:///absolute/path/plugin.tar.gz` works too. Edit config, then restart;
`run` fetches and verifies every configured package before starting, and
`pluribus run --offline` uses verified cache entries or local archives.

`bundled:<name>` names a package of the running installation instead of a
directory. `init --example` writes it for the packages this build ships, so
`nix profile upgrade` — which replaces the store path and eventually collects
it — leaves the configuration loadable. It resolves through
`PLURIBUS_PLUGIN_DIR`, then `share/pluribus/plugins` beside the binary, then
the release catalog the binary embeds.

[Async execution, inspection results, and blocking boundaries](docs/async-runtime.md).
