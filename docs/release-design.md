# Remote plugin installation

Packages support `file://` directory URLs and pinned `file://` or `https://` archive references:

```json
{
  "plugin_instances": {
    "telegram-1": {
      "package": {
        "url": "https://example.org/any/path/telegram.tar.gz",
        "sha256": "<64 lowercase hex characters>"
      }
    }
  }
}
```

The example omits instance configuration, components, aliases, and grants.
`file:///absolute/path/plugin.tar.gz` works with the same hash field. Percent-encode
spaces in URLs. File URLs must identify local regular files, not directories.
Directory URLs use a string, such as `"file:///opt/plugins/telegram"`.
Bare paths and `builtin:` references are unsupported.

The URL determines where to fetch bytes; the SHA-256 determines which bytes are
accepted. No repository naming convention, GitHub API, or catalog is required.
The archive hash lives in config. Component digests in `plugin.toml` remain
internal consistency checks.

## Commands

```sh
pluribus init
pluribus install <package>
pluribus run
pluribus run --offline
```

`run` fetches and validates every configured package before it starts any
component; `install` resolves one without starting anything.
`run --offline` fails on HTTPS cache misses; local file archives can still
be imported. Config changes take effect on restart. No confirmation or update
command is required.

Change the URL and hash to update or roll back. A new URL with the same hash
reuses cached bytes. Changed bytes with the old hash fail. Failed package/config/grant validation stops startup.

## Archive and cache

Archives are gzip-compressed POSIX ustar with `plugin.toml` at the root. Include
all components, schemas, and credential flows. No enclosing directory is needed.
The packer uses this format and produces deterministic bytes.

Downloads allow HTTPS only, with five redirects, a 15-second connection timeout,
a two-minute request timeout, and a 256 MiB archive limit. Plain HTTP, URL user
credentials, and fragments are rejected. Keep authentication secrets out of URLs;
a separate private-host credential configuration is not implemented.

Extraction rejects traversal, links, devices, duplicate paths, extension headers,
and paths over 4 KiB or 32 components. Expanded archives are limited to 512 MiB
and 10,000 entries. Archive permissions are ignored. Plugins execute only through
Wasmtime; the shell executor ships separately with the native runtime.

The cache stores the original archive and extracted package under:

```text
data/packages/sha256/<hash>/archive.tar.gz
data/packages/sha256/<hash>/package/
```

Each resolution checks the archive hash and compares extracted files against
its contents, then validates the package. Corrupt cache entries fail; they are
not silently repaired. Remove the affected cache entry while stopped to refetch.
Staging directories are renamed only after validation succeeds. Concurrent
installs may reuse an already published, verified cache entry.

Package selection comes from `config.json`; no package metadata lockfile is written.
The CLI does not lock runners or installations. Operators coordinate concurrency.

Startup resolves and validates every package before starting components.
Plugin maintainers own state compatibility and migration. Breaking changes may
use a new major package version; the host does not block version changes.
Code rollback does not undo state changes.

Backups include configuration and completed package caches, enabling offline restore.
Blob GC leaves package caches intact. Package-cache pruning is not implemented.

## Releases

| Asset | Contents |
| --- | --- |
| `pluribus-<version>-<target>.tar.gz` | `pluribus`, `pluribus-shell-executor`, `pluribus-shell-cli`, `pluribus-cli-bridge`, and `pluribus-http-listener` in `bin/` |
| `pluribus-plugin-<name>-<version>.tar.gz` | Complete WASM package |
| `plugins.json` | Versioned map of plugin names to URL/SHA-256 references |
| `<asset>.sha256` | The digest of the archive it sits beside |

Telegram's receive/send components share an archive; RLM includes JS. WASM
packages are independent of the native platform. Native builds target Linux and
macOS, each on x86-64 and ARM64.

The release workflow builds/tests plugins, packages them, then builds native
binaries with `PLURIBUS_RELEASE_CATALOG` pointing to `plugins.json`. `init`
writes an agent with no plugin instances; every package a configuration uses is
named explicitly, by directory URL or by archive URL and digest. RLM's tool
catalog and sender trust are supplied by core from the assembled instance.

A release installation needs only `bin/` and no Rust toolchain. Add it to
`PATH`, then run `pluribus init --example`. It writes each package as
`bundled:<name>`, a reference resolved at load time: from `PLURIBUS_PLUGIN_DIR`,
else a `share/pluribus/plugins/` beside the binary, else the catalog its
binaries embed, by URL and digest. An upgrade that replaces the installation
directory therefore keeps the configuration loadable. `pluribus install`
consults none of them; it requires an explicit package directory or archive URL.

Run the **Release** workflow with an existing `vX.Y.Z` tag matching the
workspace version. It creates a draft with all assets; it does not create tags
or publish the draft. Enable immutable releases in repository settings and
publish the completed draft manually. No release has been published by this work.

Local packaging:

```sh
nix-build -A release
```

One command per target. It builds every plugin package, archives each one,
writes `plugins.json` with their URLs and hashes, then builds the CLI and bridge
with that catalog embedded plus the shell executor, shell CLI, and HTTP listener.
It archives these binaries under the build's Rust target triple. The catalog is a
build input; the release also contains each plugin archive and its `.sha256` file.

Plugin components build with remapped source paths, so every runner in the
release matrix writes byte-identical plugin assets and the same catalog.
Release binaries must run off a Nix store: Linux links them statically, and the
darwin build rewrites store libraries to their system copies and re-signs.
`.github/workflows/release.yml` merges the per-target assets and opens the
draft with `gh`.

GitHub documents [immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)
and [draft publication](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository).
Release attestations and private-host authentication remain follow-up work.

## Verification

CLI tests cover file imports, offline execution/restart, config updates and
rollback, state compatibility, hash mismatches, cache tampering, archive safety,
concurrent installation, and backup/restore. The ignored loopback TLS test covers
HTTPS redirects, certificate rejection, status errors, truncation, and size limits.

`nix-build -A release` unpacks the archive it wrote and exercises it with an
empty `PATH`. It
checks installation, an empty initial configuration, file imports, offline
cache reuse, and snapshot verification. The hosted matrix
must run on GitHub; local tests cannot validate all target binaries.
