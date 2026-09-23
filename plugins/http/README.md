# HTTP listener

`http/listen` connects to `pluribus-http-listener` over its granted Unix socket.
The native listener accepts buffered HTTP requests on loopback and keeps a bounded
in-memory inbox. Caddy supplies public routing and TLS.

Native listener flags:

```sh
pluribus-http-listener \
  --listen 127.0.0.1:8090 \
  --socket /var/lib/pluribus/http.sock \
  --runtime-uid 1000 \
  --route 'github:GET,POST:/*:github/receive'
```

Repeat `--route ID:METHODS:PATH:CONSUMER` for additional routes. Methods are
comma-separated; a trailing `*` matches a path prefix. Routes are checked in order.

Install `bundled:http`, configuring `components.listen.stream.default.socket`
and `peer_uids` to match. The plugin's ordinary configuration is `{}`.

Defaults: 1 MiB request body, 32 KiB headers, 256 MiB inbox, 64 concurrent HTTP
requests, eight-second response deadline. `--max-body-bytes` supports up to 25 MiB;
`--max-queue-bytes` and `--response-timeout-seconds` override queue and deadline limits.
Set the component's stream and memory limits to accommodate configured bodies.

`http.request.received` includes `requestId`, `routeId`, `consumer`, `method`,
`target`, `receivedAtMs`, `deadlineAtMs`, `headers`, and `body`. Header entries are
`[name, base64-value]` pairs; duplicate headers are preserved. `body` is base64 of
exact request bytes. Inbound payloads are visible only to their listener and
assigned consumer. The initial protocol uses bounded inline bodies, not blob refs.

The consumer emits `http.response.requested`, causally linked to the request
event, with `requestEventId`, `status`, optional plain-text header pairs, and
optional base64 `body`. The listener checks the requesting component against the
original consumer before forwarding the response. This event is the response
interface; it is not an agent-callable capability. Responses are limited to 1 MiB.

The consumer chooses the response status. Unanswered requests time out with 504;
queue exhaustion returns 503. A response can be sent once and cannot revive a
closed connection. Acknowledged, completed, cancelled, and timed-out requests
leave the queue. Listener restarts discard queued requests and close connections;
callers must retry. A listener session ID prevents old cursors from skipping new
requests after restart. Core events provide persistence after delivery.
No state directory, streaming, or WebSocket support.

The listener pushes queue notifications over a persistent socket subscription.
The Wasm plugin awaits socket input in its async `run` loop, drains the queue,
and commits HTTP request events. It handles internal response events while
waiting for more notifications. Idle waiting produces no timer or checkpoint events. Reconnects retain
the committed queue cursor; request IDs deduplicate redelivery.
