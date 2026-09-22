# End-to-end test plan

This plan separates hermetic package/runtime integration tests from checks that
contact real services. Results are unrecorded until each case is run. The live
provider choice for this run is the OpenAI Codex subscription; setup belongs in
`target/e2e-live` so it does not use normal user data. Never place credentials in
this file or command arguments.

## Local baseline

Run from the repository root with the pinned toolchain, `wasm32-unknown-unknown`,
and loopback sockets available:

```sh
nix-shell
pluribus-sync-plugins
cargo fmt --check
cargo clippy --locked --workspace --all-targets
cargo test --locked --workspace -- --include-ignored
```

The test command matches CI. Ignored tests included here use loopback fixtures;
they do not contact providers. Do not set live-evaluation environment variables
for this run. CI also builds distribution outputs with `nix-build -A pluribus`,
`nix-build -A release`, and `nix flake check`.

| ID | Procedure | Expected assertion | Coverage | Initial status |
| --- | --- | --- | --- | --- |
| E2E-01 | Sync packages, then run the local baseline commands above. | All workspace tests pass, including loopback tests; fmt and clippy pass. | Automated; no authentication | Not run |
| E2E-02 | `cargo test --locked -p pluribus-runtime-wasm --test echo_end_to_end --test shell_denied -- --include-ignored` | Packaged echo delivery, state/restart, shell grant denial, and secret export behavior pass. | Automated fixtures | Not run |
| E2E-03 | `cargo test --locked -p pluribus-cognition --test memory --test rlm_cognition -- --include-ignored` | Memory write/recall/correction/restart; RLM event/tool handling, failures, cancellation, budget pauses, and recovery pass. | Scripted provider fixtures | Not run |
| E2E-04 | `cargo test --locked -p pluribus-plugin-http --test echo_http -- --include-ignored`; `cargo test --locked -p pluribus-host-http -- --include-ignored` | HTTP listener request crosses listener, Wasm, and agent; host HTTP grant, limits, and validation tests pass. | Loopback fixtures | Not run |
| E2E-05 | `cargo test --locked -p pluribus-cognition --test telegram_polling -- --include-ignored` | Telegram sender admission, poll offset, media retry, and deduplication behavior pass. | Mock HTTP fixture | Not run |
| E2E-06 | `cargo test --locked -p pluribus-plugin-email --test email_send -- --include-ignored`; `cargo test --locked -p pluribus-host-stream -- --include-ignored` | IMAP/SMTP transcripts, threaded reply, credential rejection, endpoint denial, TLS and STARTTLS behavior pass. | Loopback protocol fixtures | Not run |
| E2E-07 | `cargo test --locked -p pluribus-plugin-github --test github_delivery -- --include-ignored` | Signed webhook passes through listener and Wasm; secrets, enrollment replay, and token export checks pass. | Local listener and fake credentials | Not run |
| E2E-08 | `cargo test --locked -p pluribus-cli --test installation --test plugin_auth --test remote_packages -- --include-ignored`; `cargo test --locked -p pluribus-cli package_source -- --include-ignored` | Install/relocation, scoped auth, offline verified archives, cache reuse, and package selection pass. | Automated fixtures; no authentication | Not run |
| E2E-09 | `cargo test --locked -p pluribus-cli --test credential_refresh -- --include-ignored` | Codex device enrollment and token refresh work against loopback provider responses. | Loopback only; no real account | Not run |
| E2E-10 | `cargo test --locked -p pluribus-cognition --test rlm_cognition stop_signals_a_blocked_provider -- --include-ignored`; `cargo test --locked -p pluribus-host-stream -- --include-ignored` | Stop interrupts blocked work; local stream and shell execution remain within configured grants and budgets. | Loopback fixtures | Not run |
| E2E-11 | `python3 scripts/e2e-native.py` | CLI input survives bridge restart and oversized lines; escaped batches fit response limits. Shell executor returns output, workspace, and exit status. | Local binaries; no authentication | Passed |
| E2E-12 | Build distribution checks listed above when Nix is available. | Plugin packages, release archives, and flake outputs build/check successfully. | Automated; no authentication | Not run |

Treat a zero exit as pass. Record compiler/clippy warnings separately; warnings
do not count as test failures unless the command exits nonzero.

## Isolated local runtime

Use the standalone profile in `target/e2e-live`; it uses local debug binaries,
packages under `target/plugins`, its own SQLite state, and a shell workspace.
From the repository root, run `./target/e2e-live/setup.sh` after building and
syncing packages. It writes the local config and does not enroll credentials.
Start these in separate terminals:

```sh
./target/debug/pluribus-cli-bridge --data-dir "$PWD/target/e2e-live/data"
./target/debug/pluribus-shell-executor --data-dir "$PWD/target/e2e-live/data" --workspace "$PWD/target/e2e-live/workspace"
./target/debug/pluribus --data-dir "$PWD/target/e2e-live/data" run
```

The executor uses the current OS account, with its working directory set to the
isolated workspace. Authenticate the user-selected Codex subscription while
the runtime is active:

```sh
./target/debug/pluribus --data-dir "$PWD/target/e2e-live/data" auth openai-codex
```

Complete the displayed device flow at its verification URL. Authentication and
model calls contact OpenAI. Do not save the one-time code in the workspace.

| ID | Procedure | Expected assertion | Coverage | Initial status |
| --- | --- | --- | --- | --- |
| LIVE-01 | Run `./target/e2e-live/setup.sh`, then start the three local processes above. | Local package URLs resolve, components load, and the bridge/executor/runtime connect using sockets under the isolated data directory. | Local binaries and packages; no authentication | Not run |
| LIVE-02 | With runtime active, run `./target/debug/pluribus --data-dir "$PWD/target/e2e-live/data" auth openai-codex`; complete the displayed device flow. | Enrollment succeeds and secrets remain sealed in isolated state. | OpenAI Codex subscription; user authentication required | Not run |
| LIVE-03 | In the bridge, send `Reply with exactly: live e2e ok`. | The Codex provider returns the requested text through the CLI. | Live provider request; may consume subscription quota | Not run |
| LIVE-04 | Send `Use the shell to run pwd and printf 'shell-ok\\n'; report the output.` then `Remember that the live E2E marker is violet.` Ask `What is the live E2E marker?`; stop the runtime with `./target/debug/pluribus --data-dir "$PWD/target/e2e-live/data" stop`, restart with `run --resume`, and ask again. | Shell output is from the isolated workspace; memory recalls the marker before and after restart. | Live provider, shell, memory, persistence | Not run |

Do not run connector cases below without the corresponding test account and a
safe destination. Sending email/messages and changing a webhook are external
side effects; these require an explicit target chosen by the user.

## Optional live connector cases

| ID | Procedure | Expected assertion | Coverage | Initial status |
| --- | --- | --- | --- | --- |
| LIVE-05 | Enroll a test Telegram bot, allow one test sender, send text and media; also send from a sender outside the allowlist. | Authorized updates produce observations; unauthorized updates produce none; media is attached once. | Live Telegram authentication | Not run |
| LIVE-06 | Enroll a test mailbox with an app password; observe a newly received message, then reply only to a user-selected safe test address. | IMAP observation and SMTP delivery succeed; reply threading and recipient are correct. | Live email authentication and delivery | Not run |
| LIVE-07 | Enroll a GitHub App installed on a test repository; expose the configured listener over HTTPS and deliver a signed ping and test event. | Ping returns HTTP 200; valid delivery is observed once; duplicate delivery is deduplicated; invalid signature is rejected. | Live GitHub authentication, public HTTPS endpoint | Not run |

## Limits

The workspace suite exercises packaged Wasm components and native listeners
against fixtures, including connector protocol transcripts. It does not prove
real provider availability, account permissions, live mailbox behavior, bot
delivery, GitHub webhook routing, or model quality. The optional live memory
evaluation is separate and is not part of the Codex smoke test; see
[`memory-evaluations.md`](memory-evaluations.md).
