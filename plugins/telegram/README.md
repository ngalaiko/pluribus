# Telegram connector

One package contains two independent components. `receive` owns polling and the update offset; `send` owns outbound capabilities. Both use the package bot-token credential. A blocked poll does not block sending. It declares bot-token enrollment in its manifest. The host seals the token as a path credential; component memory receives only its handle.

Set `plugin_instances.telegram.config.trusted_senders` to Telegram user IDs as strings:

```json
{"package":"bundled:telegram","config":{"trusted_senders":["123456789"]}}
```

Only listed senders are admitted. An empty or omitted list ignores everyone. Ignored updates advance the polling offset but produce no observations, replies, or media downloads. Pending media from removed senders is discarded. Observations omit the raw poll batch so other senders' updates are not exposed through it.

Inbound normalization covers text, photos, documents, audio, voice, video, video notes, animations, stickers, locations, venues, contacts, polls, dice, replies, quotes, forwards, edits, reactions, callbacks, and album identifiers.

The receiver owns an async polling loop; `poll_timeout_seconds` controls each Telegram long poll. Empty responses create no events. The loop commits the update offset before issuing its next request. Startup resumes from that offset. Errors use bounded backoff; successful polls wait at least 100 ms before repeating. Waits create no timer events. Text-only updates emit observations immediately. Media updates commit their payload and offset to a durable download queue. Downloads run in round-robin order, one file per poll, with at least 30 seconds between retries and at most five attempts. Successful downloads remain stored while other attachments retry. Once every attachment is ready or permanently failed, the receiver emits one `observation.received` containing the message and final media metadata, then removes the pending entry in the same commit. Failed attachments retain error details. Telegram serves files as `application/octet-stream`; the ready blob takes its media type from the attachment `mime_type`, else the attachment kind, else the file extension. Observations use `chat:<id>` conversation identity and omit `message_thread_id`.

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
