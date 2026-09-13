# GitHub

The Wasm plugin handles App enrollment, JWT signing, token refresh, and webhook
verification. It uses core-owned credential storage and the HTTP plugin.
There is no GitHub daemon or separate credential database.

## Configuration

```json
{
  "package": "bundled:github",
  "config": {
    "credentials": {"app": "github:personal"},
    "http_instance": "http/listen",
    "route_id": "github",
    "owner": "ngalaiko"
  }
}
```

Create a private GitHub App under your personal account. Set its webhook URL to
`https://computer.tail4fad0.ts.net/nikita/pluribus/github/events` and choose a
webhook secret. Configure the repository permissions and event subscriptions
listed in `app.flow.json`. Download the App's private key and note its App ID.
No OAuth callback or setup URL is required.

With `pluribus run` active, run:

```sh
pluribus auth github
```

Enter the App ID, the path to the downloaded PEM file, and the webhook secret.
The CLI stages the input in private core credential storage; the event contains
only an enrollment reference. GitHub Wasm validates the key with `GET /app`,
checks the owner, stores the credentials, and returns the installation URL.
Open it and approve access to **all repositories** on your personal account.
The plugin discovers the installation and refreshes tokens automatically.

For noninteractive enrollment, stdin accepts a JSON object with `app_id`,
`private_key` (PEM contents), and `webhook_secret`. Do not put secrets in arguments.
A webhook secret cannot be checked through `GET /app`; it must match the App's
webhook configuration. Signed deliveries verify it.

Route the public prefix to the listener, stripping the prefix. Assign `GET` and
`POST` under `/*` to `github/receive`. Allow outbound `GET` and `POST` to
`https://api.github.com`. Other HTTP paths return 404.

## Shell access

The plugin refreshes installation tokens before expiry and publishes the
`installation-token` export. Set the shell instance's `config`:

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

Shell Wasm resolves `GH_TOKEN` through the core and sends it over the executor
socket. The grant exposes only the named export, with at least 30 seconds
remaining. It does not grant access to the App key or webhook secret.
Install `gh` on the executor's PATH; GitHub mutations use shell commands.

## Observations

The plugin verifies HMAC-SHA256 over exact request bytes, checks the repository
owner and installation, and emits `observation.received` with `trusted: false`.
Unsigned requests return 401; wrong owners or installations return 403.
A committed delivery ID is acknowledged without another observation; reuse
with different bytes returns 409.

Deduplication scans committed GitHub observations. Large histories will need a
durable indexed lookup. Token refresh checks installation access every minute;
failed checks clear the published export. Already issued tokens retain their
GitHub-side lifetime.

## Tests

```sh
cargo test --locked -p pluribus-plugin-github
```

Tests cover RSA signatures, exact-byte HMAC verification, and signed requests
through the native HTTP listener, both Wasm components, and core secret storage.
