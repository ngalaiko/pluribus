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
GitHub Wasm refreshes an installation token and exposes the webhook secret →
core credential storage → shell Wasm resolves `GH_TOKEN` and
`GITHUB_WEBHOOK_SECRET` → executor socket → command environment → `gh`.

Repository setup uses the same `/events` URL as App deliveries. `gh` lists
hooks with `--paginate`, updates the hook whose URL matches the configured
receiver, or creates it. It sends `GITHUB_WEBHOOK_SECRET` through stdin via
`config[secret]=@-`; the value must not appear in arguments, output, or logs.
The installation token needs repository hooks write access. User and
organization installations are supported, including selected repositories.
After each upsert, ping the hook and verify its delivery. Check existing App
subscriptions first because overlapping event subscriptions can duplicate
deliveries.

See [GitHub](../plugins/github/README.md), [HTTP](../plugins/http/README.md),
and [credentials](plugins/credentials.md) for configuration and contracts.
