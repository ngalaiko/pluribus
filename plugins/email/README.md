# Email

Observes arriving mail over IMAP and answers it over SMTP. One component, two
endpoints: the host holds each connection; the component speaks IMAP, parses
and writes MIME, and emits one observation per message.

No polling: the session sits in `IDLE` and the server pushes. A mailbox with
no `IDLE` support fails at startup with a clear error rather than degrading
into a poll nobody configured.

## Install

```sh
pluribus plugins install email
pluribus plugins auth email
```

`auth` prompts for the username and an app-specific password, and seals them
under the instance's credential handle. Configuration holds the handle; the
secret is never written to it. One credential authenticates both endpoints:
IMAP `LOGIN` and SMTP `AUTH` present the same username and password.

iCloud requires an app-specific password, generated at
[account.apple.com](https://account.apple.com) under Sign-In and Security. The
account password will not authenticate an IMAP session.

## Grants

The manifest asks for `net.tls` twice, and installation writes exactly those
endpoints:

```json
{
  "plugin_instances": {
    "email": {
      "package": "bundled:email",
      "access": {
        "stream": {
          "imap": {
            "tls": {
              "hostname": "imap.mail.me.com",
              "port": 993
            },
            "max_bytes": 1073741824,
            "max_timeout_ms": 15000,
            "max_connections": 1
          },
          "smtp": {
            "tls": {
              "hostname": "smtp.mail.me.com",
              "port": 587,
              "starttls": "smtp"
            },
            "max_bytes": 33554432,
            "max_timeout_ms": 30000,
            "max_connections": 1
          }
        }
      }
    }
  }
}
```

`connect` takes an endpoint name, not a destination: the component reaches
`imap` and `smtp` and nothing else, and a name the grant does not carry is
denied. Changing a host or port in configuration is refused at startup unless
the manifest asks for it: an endpoint grant may not exceed what was declared.

`max_bytes` is one connection's whole budget across both directions.
Exhausting the IMAP budget ends the connection, which the plugin treats as a
reconnect and resumes from its cursor — so it bounds a session's lifetime
rather than losing mail. The SMTP budget bounds one message, because each send
gets its own connection.

Another provider needs its own package with its own declared endpoints. That
is deliberate — the endpoint is the authority.

### Why the host speaks STARTTLS

`smtp.mail.me.com:587` answers in plaintext and upgrades on request; Apple
documents no implicit-TLS port beside it. The grant names `starttls: "smtp"`
and the host performs the whole preamble — greeting, `EHLO`, `STARTTLS`,
`220` — before handing this component a connection that is already encrypted.

That the guest never sees a plaintext byte is a property of the grant, not of
the protocol. A component holding the plaintext connection could simply
decline to upgrade and nothing above it would know, so the host owns the
upgrade and there is no way to opt out. What the component does own is the
whole of SMTP from its own `EHLO`, which RFC 3207 requires after the handshake
anyway. See [byte channels](../../docs/plugins/stream.md#starttls).

## Configuration

| Field | Meaning |
| --- | --- |
| `mailbox` | The mailbox observed. One instance watches one mailbox. Defaults to `INBOX`. |
| `max_message_bytes` | Largest message fetched whole. A larger one is observed by its header and marked `truncated`. Defaults to 2 MiB. |
| `max_attachment_bytes` | Largest attachment stored as a blob. A larger one is described without its bytes. Defaults to 8 MiB, above the message ceiling, so `max_message_bytes` is the binding limit unless this is lowered. |
| `batch` | Messages fetched before committing and looking around. Defaults to 20. |
| `import_history` | Observe what is already in the mailbox on first connection. Off by default. |
| `from` | The address outgoing mail is sent from, envelope and header both. Defaults to the credential's username, which for iCloud is the account's address. |
| `display_name` | The display name outgoing mail carries beside the address. |

## What it does to the mailbox

Nothing. `EXAMINE` opens the mailbox read-only and `BODY.PEEK` leaves `\Seen`
alone, so observing mail does not mark it read. Sending never touches the
mailbox either: submission is a separate connection to a separate endpoint,
and nothing is written back to the account.

That is a property of this code, not of the grant. The credential is a working
password: anything holding it can delete mail. A TLS endpoint grant cannot
express "read-only", and the host does not inspect IMAP commands. If the
distinction matters, give the connector its own account.

## Delivery

Each message is identified by mailbox, `UIDVALIDITY` and `UID`. That tuple is
the observation's idempotency key, so a redelivered batch deduplicates instead
of arriving twice. Sequence numbers are not identity — they shift under
expunges and mean nothing across reconnects. [RFC 9051
§2.3.1.1](https://www.rfc-editor.org/rfc/rfc9051.html#section-2.3.1.1)

The cursor commits atomically with the observations it covers, so a crash
between fetch and commit repeats the fetch rather than losing the mail.

A first connection records where the mailbox stands and observes what arrives
after. Importing the existing mailbox is `import_history: true`.

When `UIDVALIDITY` changes the server has renumbered the mailbox and every
stored UID is meaningless. The connector re-records the boundary rather than
reusing the old cursor, and emits a `mailbox-resynchronized` observation
saying so: the gap that creates is visible rather than silent.

## Session

1. Connect the `imap` endpoint, greet, `CAPABILITY`, `LOGIN`, `EXAMINE`.
2. Catch up from the cursor with `UID SEARCH` and `UID FETCH`, in batches.
3. `IDLE`.
4. On an `EXISTS` or `RECENT` push, leave IDLE, fetch, commit, resume.
5. Renew IDLE every 25 minutes.
   [RFC 2177](https://www.rfc-editor.org/info/rfc2177/) allows 29.
6. On a dropped connection, reconnect with bounded backoff and catch up from
   the cursor.

The IDLE wait is a 30-second read, not one 25-minute block: each tick is where
a stop is noticed and the renewal clock is checked. While waiting the component
still services internal deliveries, which is where sending happens.

A rejected password ends the source rather than retrying — it will be rejected
again, and hammering an endpoint with a bad credential locks accounts.

## Observations

`observation.received` with schema `dev.pluribus.email.observation.v1`. Body
text, subject, addresses and attachment names are written by whoever sent the
message, so the payload carries `trusted: false`.

| Field | Meaning |
| --- | --- |
| `externalSenderId` | The sender's address, lowercased. |
| `conversationId` | `email:<mailbox>`. Mail has no per-thread channel to reply into. |
| `uid`, `uidValidity`, `mailbox` | Message identity. |
| `subject`, `from`, `to`, `cc`, `replyTo`, `date`, `messageId`, `inReplyTo` | Header fields, decoded from RFC 2047 encoded words and bounded. |
| `references` | The thread's earlier message identifiers, brackets removed, so a reply can continue the chain. |
| `message.text` | The plain-text part, or the HTML part reduced to text. Bounded at 256 KiB. |
| `bodyFromHtml` | Whether `message.text` came from HTML. |
| `attachments` | Name, media type, size, and a blob reference. An attachment past the ceiling is described with `status: "too-large"` and no blob. |
| `truncated` | The message exceeded `max_message_bytes` and only its header was fetched. |

## Sending

| Capability | What it does |
| --- | --- |
| `email.reply` | Answers the message an observation came from. Every connector provides `<provider>.reply`. |
| `email.send-message` | Sends to explicit recipients. Starts a thread rather than continuing one. |

Both answer with `capability.completed` carrying the `Message-Id` written and
the recipients the endpoint accepted, or `capability.failed` carrying a code
and the endpoint's own words.

`email.reply` takes the observation's own fields: `conversationId`, `to` —
which is the observation's `replyTo` when it had one and its `from` otherwise
— `messageId`, `references`, `subject`, and the `text` to send. It threads by
putting `messageId` in `In-Reply-To` and at the end of `References`, and
prefixes the subject with `Re: ` unless it already says so. That argument
shape is what confines it: a grant issued for one conversation carries
`conversation_ids`, and a request naming another is refused before it reaches
this component.

`email.send-message` takes `to`, optional `cc`, an optional `subject` and
`text`. It names no conversation, so a grant constrained to one will not
authorize it — an agent answering an untrusted sender can reply and nothing
more.

Each send opens its own connection and closes it with `QUIT`. Holding a second
long-lived connection open beside the IMAP one to save a handshake on a rare
command is not worth what it costs. The IMAP session is suspended while a send
runs and resumes where it was: no command is interleaved, and the cursor is
untouched.

Messages are written as RFC 5322 with CRLF throughout. A non-ASCII subject or
display name travels as RFC 2047 encoded words; a body that cannot travel as
it stands — non-ASCII, long lines, trailing whitespace — travels as
quoted-printable, and one that can travels as `7bit`. Header values are
encoded rather than interpolated, so nothing a caller supplies can become a
header of its own, and an address is refused unless it is a bare addr-spec.

### What a failure means

| Endpoint reply | Code | Meaning |
| --- | --- | --- |
| `4xx` | `unavailable` | The endpoint asked to be tried later. Retrying is reasonable. |
| `530`, `534`, `535`, `538` | `permission-denied` | The credential was rejected. Re-run `pluribus plugins auth`; retrying will not help. |
| `523`, `552` | `resource-exhausted` | The message is too large for the endpoint. |
| other `5xx` | `invalid-argument` | The endpoint refused the message itself — an unknown recipient, usually. It will refuse it identically on every attempt. |

A recipient refused at `RCPT TO` ends the submission before `DATA`: a partly
accepted recipient set would deliver a message nobody asked for.

## Limits

One mailbox per instance. No attachments on outgoing mail, no HTML bodies, no
searching: the send side writes one plain-text part. Nothing is saved to a
Sent mailbox — submission and the observed mailbox are separate endpoints, and
writing to the second would be the first write this connector has ever made.
