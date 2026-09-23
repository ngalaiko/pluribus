# GitHub architecture

- `plugins/http/listener`: native loopback HTTP server and in-memory request inbox.
- `plugins/http/src`: Wasm adapter that emits HTTP request events and sends responses.
- `plugins/github/src`: Wasm App enrollment, RSA signing, token refresh,
  HMAC verification, and GitHub observations.
- Host credential storage: private records scoped by package ID and handle.
- Shell Wasm: resolves granted exports through the host and binds environment names.
- Shell executor: receives environment values over its granted socket.

The host contains no GitHub-specific credential code. GitHub has no native
process or separate database. The HTTP listener and shell executor take flags.

Inbound route:
GitHub → public HTTPS/Caddy → HTTP listener → HTTP Wasm → host events → GitHub Wasm.

Enrollment:
`pluribus plugins auth github` → App ID, private key, webhook secret → private host storage →
enrollment reference → GitHub Wasm validates App → installation approval URL.

Shell authentication:
GitHub Wasm refreshes an installation token and exposes the webhook secret →
host credential storage → shell commands request `GH_TOKEN` or
`GITHUB_WEBHOOK_SECRET` through `pluribus-shell-cli secret`.

See [GitHub](README.md), [HTTP](../http/README.md),
and [credentials](../../docs/plugins/credentials.md) for configuration and contracts.
