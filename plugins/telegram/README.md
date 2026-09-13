# Telegram connector

One package contains two independent components. `receive` owns polling and the update offset; `send` owns outbound capabilities. Both use the package bot-token credential. A blocked poll does not block sending. It declares bot-token enrollment in its manifest. The host seals the token as a path credential; component memory receives only its handle.

Set `plugin_instances.telegram.config.trusted_senders` to Telegram user IDs as strings:

```json
{"package":"bundled:telegram","config":{"trusted_senders":["123456789"]}}
```

Only listed senders are admitted. An empty or omitted list ignores everyone. Ignored updates advance the polling offset but produce no observations, replies, or media downloads. Pending media from removed senders is discarded. Observations omit the raw poll batch so other senders' updates are not exposed through it.

Inbound normalization covers text, photos, documents, audio, voice, video, video notes, animations, stickers, locations, venues, contacts, polls, dice, replies, quotes, forwards, edits, reactions, callbacks, and album identifiers.

Core holds a granted HTTP subscription; `poll_timeout_seconds` controls the Telegram long poll. Empty responses create no events. The receive callback commits the update offset before replacing the subscription request. Startup restores the subscription from that offset; timer events are unused. Intake commits observations, media descriptors, and the update offset before downloading attachments. Pending media is stored per update and retried in round-robin order, one file per poll, with at least 30 seconds between attempts and at most five attempts. Successful downloads remain stored while other attachments retry. Completion emits `telegram.media-ready` or `telegram.media-failed`, preserving the original observation fields and `observationDeduplicationKey`; it does not create another observation.

Outbound capabilities:

- `telegram.send-message`
- `telegram.send-draft`
- `telegram.send-media`
- `telegram.send-media-group`
- `telegram.send-location`
- `telegram.react`
- `telegram.edit-message`
- `telegram.delete-message`

Each component is its own crate, `receive` and `send`, over the transport and configuration code in `core` (`pluribus-plugin-telegram-core`). `poll_timeout_seconds` applies only to `receive`.
