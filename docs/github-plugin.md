# GitHub plugin architecture

- `plugins/http/listener`: native loopback HTTP server and in-memory request inbox.
- `plugins/http/src`: Wasm adapter that emits HTTP request events and sends responses.
- `plugins/github/src`: Wasm App enrollment, RSA signing, token refresh,
  HMAC verification, and GitHub observations.
- Core credential storage: private records scoped by package ID and handle.
- Shell Wasm: resolves granted exports through the core and binds environment names.
- Shell executor: receives environment values over its existing socket.

The core contains no GitHub-specific credential code. GitHub has no native
process or separate database. The HTTP listener and shell executor take flags.

Inbound route:
GitHub → public HTTPS/Caddy → HTTP listener → HTTP Wasm → core events → GitHub Wasm.

Enrollment:
`pluribus auth github` → App ID, private key, webhook secret → private core storage →
enrollment reference → GitHub Wasm validates App → installation approval URL.

Shell authentication:
GitHub Wasm refreshes an installation token → core credential storage →
shell Wasm resolves `GH_TOKEN` → executor socket → command environment → `gh`.

See [GitHub](../plugins/github/README.md), [HTTP](../plugins/http/README.md),
and [credentials](plugins/credentials.md) for configuration and contracts.
