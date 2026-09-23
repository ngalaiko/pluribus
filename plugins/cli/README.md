# CLI

A person at a terminal, as a connector. The component subscribes to bridge input over its
granted [byte stream](../../docs/plugins/stream.md) and receives no process,
filesystem, environment, or credential access besides that one endpoint.

One package contains both the Wasm component and the bridge binary, as the
shell plugin does.

Observations carry `provider: "cli"`, the configured conversation and sender,
and `message.text`. The component provides `cli.reply`, the answer every
connector provides; it has no media capabilities, so an agent reached this way
answers in text.

## Run

```sh
pluribus-cli-bridge
```

The bridge owns the terminal: it numbers each line typed, hands them to the
agent through long polling, and prints replies. Empty responses create no events;
input waits run independently of reply connections. It listens; the component connects.
Each bridge run has its own session ID, so restarting it resets the saved input
cursor and accepts new lines even though sequence numbers start again at one.
Input lines are limited to 16 KiB. Poll batches include only messages whose
fully escaped JSON response fits the 256 KiB frame limit; remaining lines are
sent by later polls.
Protocol v2 requires upgrading the bridge and component together.

Its endpoint is `cli-main.sock` in the platform runtime directory;
`--data-dir` groups it with agent state and `--socket` names another path.
See [paths](../../docs/operations.md#paths). `--runtime-uid` is the account the agent runs as, and defaults to
this one.

## Configure

```json
{
  "package": "file:///path/to/plugins/cli",
  "config": {
    "conversation_id": "local",
    "sender": "operator",
    "poll_timeout_seconds": 20
  },
  "components": {
    "main": {
      "stream": {
        "default": {
          "socket": "/tmp/pluribus-cli.sock",
          "peer_uids": [501]
        }
      }
    }
  }
}
```

`peer_uids` lists the accounts allowed to answer. The bridge runs as the person
at the keyboard, which is the account the runtime uses, so that UID is listed
here. The endpoint only carries text in and out, so it cannot act with that
authority. An endpoint that can act, such as the shell executor, belongs on its
own account unless you are trying things out. `pluribus init --example` writes
all of this.

CLI observations receive the agent's configured grants.
A poll waits up to `poll_timeout_seconds` for input, so a reply appears as soon
as the agent produces one.
