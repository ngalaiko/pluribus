# GitHub

The Wasm plugin handles App enrollment, JWT signing, token refresh, and webhook
verification. It uses core-owned credential storage and the HTTP plugin.
There is no GitHub daemon or separate credential database.
See [architecture](architecture.md) for enrollment and credential flow.

## Configuration

```json
{
  "package": "bundled:github",
  "config": {
    "credentials": {"app": "github:personal"},
    "http_instance": "http/listen",
    "route_id": "github",
    "owner": "your-owner"
  }
}
```

Create a GitHub App. Set its webhook URL to
`https://agent.example.org/github/events` and choose a
webhook secret. Configure the repository permissions and event subscriptions
listed in `app.flow.json`. Download the App's private key and note its App ID.
No OAuth callback or setup URL is required.

With `pluribus run` active, run:

```sh
pluribus plugins auth github
```

Enter the App ID, the path to the downloaded PEM file, and the webhook secret.
The CLI stages the input in private core credential storage; the event contains
only an enrollment reference. GitHub Wasm validates the key with `GET /app`,
stores the credentials, and returns the installation URL. Install it on the
configured `owner`, a user or organization, with all or selected repositories.
The plugin discovers that installation and refreshes tokens automatically.
Use separate credential handles, routes, and shell instances for different
installation owners.

For noninteractive enrollment, stdin accepts a JSON object with `app_id`,
`private_key` (PEM contents), and `webhook_secret`. Do not put secrets in arguments.
A webhook secret cannot be checked through `GET /app`; it must match the App's
webhook configuration. Signed deliveries verify it.

Route the public prefix to the listener, stripping the prefix. Assign `GET` and
`POST` under `/*` to `github/receive`. Allow outbound `GET` and `POST` to
`https://api.github.com`. Other HTTP paths return 404.

## Shell access

The plugin refreshes installation tokens before expiry and publishes the
`installation-token` and `webhook-secret` exports. Set the shell instance's
`config`:

```json
{
  "credential_exports": {
    "GH_TOKEN": {
      "credential": "github:personal",
      "provider": "dev.pluribus.github",
      "export": "installation-token"
    },
    "GITHUB_WEBHOOK_SECRET": {
      "credential": "github:personal",
      "provider": "dev.pluribus.github",
      "export": "webhook-secret"
    }
  }
}
```

Shell Wasm resolves both values through the core and sends them over the
executor socket. The grant exposes only the named exports, with at least 30
seconds remaining. Install `gh` on the executor's PATH; GitHub mutations use
shell commands. Commands must never print `GITHUB_WEBHOOK_SECRET`.
Refresh backfills the secret export for existing enrollments and renews its
ten-minute lease. Upgrade the plugin before enabling the shell binding; an
unavailable export prevents shell execution.

### Install a repository hook

The same `/events` URL accepts App and repository deliveries. `GH_TOKEN` must
be an installation token with repository hooks write access. The installation
may be for a user or organization and may select specific repositories.

For each repository, list hooks with pagination, then update the matching
Pluribus URL or create it if absent. This keeps unrelated hooks intact and is
safe to repeat:

```sh
set -eu
repo=${1:?owner/repository}
url=${GITHUB_WEBHOOK_URL:?public webhook URL}
: "${GITHUB_WEBHOOK_SECRET:?webhook secret export unavailable}"
hooks=$(gh api --paginate "repos/$repo/hooks" --jq '.[] | [.id, .config.url] | @tsv')
hook_id=
while IFS="$(printf '\t')" read -r id hook_url; do
  [ "$hook_url" = "$url" ] || continue
  [ -z "$hook_id" ] || { printf '%s\n' 'Multiple matching hooks' >&2; exit 1; }
  hook_id=$id
done <<EOF
$hooks
EOF

hook_path="repos/$repo/hooks"
[ -n "$hook_id" ] && hook_path="$hook_path/$hook_id"
method=POST
[ -n "$hook_id" ] && method=PATCH

hook_id=$(printf '%s' "$GITHUB_WEBHOOK_SECRET" |
  gh api --method "$method" "$hook_path" \
    -F name=web \
    -F active=true \
    -F 'events[]=push' \
    -F 'events[]=deployment' \
    -F 'events[]=deployment_status' \
    -F 'events[]=pull_request' \
    -F 'events[]=pull_request_review' \
    -F 'events[]=pull_request_review_comment' \
    -F 'events[]=check_run' \
    -F 'events[]=check_suite' \
    -F 'events[]=workflow_run' \
    -F 'events[]=workflow_job' \
    -F 'events[]=issues' \
    -F 'events[]=issue_comment' \
    -F 'events[]=release' \
    -F "config[url]=$url" \
    -F 'config[content_type]=json' \
    -F 'config[insecure_ssl]=0' \
    -F 'config[secret]=@-' --jq '.id')

gh api --method POST "repos/$repo/hooks/$hook_id/pings"
gh api "repos/$repo/hooks/$hook_id/deliveries" \
  --jq '.[] | select(.event == "ping") | {id, guid, delivered_at, status_code}'
```

The secret travels on stdin and never appears in the command arguments. After
an upsert, wait for the new ping's delivery to report HTTP 200; an older
successful ping is insufficient. On an ambiguous mutation failure, re-list
hooks before retrying. Existing App subscriptions can overlap these events;
inspect the App settings before enabling repository hooks. Independent hooks
can produce distinct delivery IDs for the same activity.

## Observations

The plugin verifies HMAC-SHA256 over exact request bytes, checks the repository
owner and installation, and emits `observation.received` with `trusted: false`.
Repository deliveries require membership in the installation's repository list;
their optional installation ID must match. Membership lookup failures return 503.
Unsigned requests return 401; wrong owners or installations return 403.
A committed delivery ID is acknowledged without another observation; reuse
with different bytes returns 409.

Deduplication scans committed GitHub observations. Lookup cost grows with history. Token refresh checks installation access every minute;
failed checks clear both published exports. Already issued tokens retain their
GitHub-side lifetime.

## Tests

```sh
cargo test --locked -p pluribus-plugin-github
```

Tests cover RSA signatures, exact-byte HMAC verification, and signed requests
through the native HTTP listener, both Wasm components, and core secret storage.
