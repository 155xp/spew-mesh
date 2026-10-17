# Protocol

spew mesh speaks two protocols: calls between devices, over iroh links, and JSON-RPC on the local control socket
between the daemon and its clients (the MCP server, hooks, serve and the CLI). This page summarizes both. Sections
5 and 6 of [design.md](design.md) are the full contract; section numbers below point there.

## Between devices

### Links (5.1 to 5.3)

- Each device is an iroh endpoint whose id is its ed25519 public key. Links use the ALPN `spew-mesh/1`.
- Right after the TLS handshake, both sides check that the other endpoint id is admitted by the roster. If not,
  the connection is closed with application code 1.
- One link per pair of devices. The device whose endpoint id is greater (byte order) is the designated dialer and
  keeps the link up. The other dials only when it has mail and no link has come up for 30 seconds. If two links
  open anyway, both sides keep the designated dialer's and close the other with code 2.
- Dialing backs off from 3 seconds, doubling up to 5 minutes, with ±20% jitter. One attempt gives up after 15
  seconds.
- QUIC keep-alive (5 s) and idle timeout (30 s) tell when a device is gone. There is no extra heartbeat.

Application close codes:

| Code | Name | Meaning |
|---|---|---|
| 1 | `not-admitted` | The roster doesn't admit this endpoint id. |
| 2 | `duplicate` | Another link between the same two devices won. |
| 3 | `version` | The two devices share no protocol version. |
| 4 | `shutdown` | The daemon is stopping. |

### Calls and frames (5.4)

Each call uses its own bidirectional QUIC stream. The caller writes one request frame and finishes its side; the
callee writes one response frame and finishes.

- A frame is a 4-byte big-endian length, then that many bytes of UTF-8 JSON, at most 128 KiB.
- The request must arrive within 10 seconds of the stream opening, or the stream is reset.
- At most 16 inbound streams per link are open at once.
- A request is a JSON object whose `t` names the call; the call's fields sit next to it.
- Errors come back as `{"t":"error","code":"..."}` with the codes `malformed`, `too-large`, `unknown-call` and
  `hello-required`. An unknown `t` gets `unknown-call`, so newer peers can probe.

| `t` | Purpose | Response |
|---|---|---|
| `hello` | Versions and features, first on every new link | `hello` |
| `presence` | The sender's presence snapshot, filtered for this peer | `ok` |
| `deliver` | One message | `receipt` with `state` `accepted`, `duplicate` or `refused` |
| `receipt` | A later state of a message the peer sent: `shown`, `answered`, `expired` or `refused` | `ok` |
| `ping` | Round-trip check | `pong` |

`hello`, sent by the dialer on the link's first stream and answered in kind:

```json
{"t":"hello","proto_min":1,"proto_max":1,"app":"spew-mesh/0.1.0","device_name":"sam-mac","features":[]}
```

Until the `hello` exchange completes, any other request gets `hello-required`. The link uses the highest version
both sides support.

### Messages (5.5)

```json
{
  "t": "deliver",
  "id": "01JA2Q...",
  "conversation": "01JA2Q...",
  "kind": "ask",
  "in_reply_to": null,
  "from": {"session": "01JA2P...", "name": "claude-api", "client": "claude"},
  "to": {"session": "01JA2N...", "name": "claude-web"},
  "body": "Does the billing API take cents or dollars?",
  "card": "spew#40",
  "repo": "acme/api",
  "sent_at": 1760000000000,
  "ttl_secs": 86400
}
```

- `kind` is `ask`, `tell` or `answer`. Ids are ULIDs chosen by the sending device. A conversation's id is the id of
  its first message.
- `from.session` is always a session on the sending device. The sender's person and device come from the link and
  the roster, never from the payload, and nothing in the payload names a room.
- The receiver checks, in order: admission; a rate limit of 120 `deliver` calls per rolling minute per device,
  counted before parsing; the shape (field sets, lengths, a body of 1 to 32768 bytes after sanitizing, `ttl_secs`
  1 to 604800, no unknown fields, `from.name` not `mesh`); dedupe; threading (a reply must answer a message this
  device sent to that device, and an `ask` gets at most one `answer`); the target session and whether the sender
  may see it; and a guard of 24 messages per conversation per 15 minutes.

The `deliver` response:

```json
{"t":"receipt","id":"01JA2Q...","state":"refused","code":"no-such-session"}
```

| Refusal code | Meaning | Sender retries? |
|---|---|---|
| `rate-limited` | Over 120 calls a minute | Yes; the message stays queued |
| `invalid` | Bad shape or threading | No |
| `no-such-session` | No such session, or the sender may not see it | No |
| `session-ended` | The target session has ended | No |
| `busy` | A responder's queue is full or it is over its run limits | No |
| `conversation-limit` | 24 messages in 15 minutes in this conversation | No |

A later `receipt` call, from the device that holds the message:

```json
{"t":"receipt","id":"01JA2Q...","state":"shown","at":1760000005000}
```

### Presence (5.6)

```json
{
  "t": "presence",
  "seq": 42,
  "device": {"name": "maya-mbp", "platform": "macos", "version": "0.1.0"},
  "sessions": [
    {"session": "01JA2N...", "name": "claude-web", "client": "claude", "kind": "live",
     "repo": "acme/web", "branch": "billing", "place": "web", "room": "spew",
     "cards": ["spew#40"], "activity": "Adding refunds",
     "state": "busy", "since": 1760000000000, "reach": "hooked", "queue": 0}
  ]
}
```

- Sent when a link comes up, after a local change (debounced 300 ms), and at least every 45 seconds. A receiver
  takes at most one per second per device.
- Filtered per peer. Your own devices get everything. A teammate's device gets only sessions in rooms it shares,
  only the cards of that room's project, and `branch` and `place` only for rooms that came from `.spew.json`.
  Terminal sessions are never sent.
- `repo` is the last two segments of the `origin` remote's path, never its host or user. `place` is the last
  component of the working folder, never a whole path.
- `reach` is `hooked`, `inbox` or `serve`. `queue` is a responder's number of waiting questions.

## The control socket (6)

The daemon listens on a Unix socket: `~/.spew/mesh/run/ipc.sock` when that path is shorter than 100 bytes, else a
randomly named socket in `$XDG_RUNTIME_DIR`, `$TMPDIR` or `/tmp`. The chosen path is written to
`~/.spew/mesh/run/socket`. The socket is mode 0600, and both ends check the other's user id.

The protocol is JSON-RPC 2.0, one JSON object per line in each direction. Requests may overlap on one connection.

| Method | Does |
|---|---|
| `session.open` | Opens a session (live, responder or terminal) and returns its address and a token |
| `session.update` | Updates a session's folder, state, activity or cards |
| `session.close` | Ends a session |
| `hook.event` | One hook event: start, prompt, tool, stop or end; returns the context to show |
| `messages.ack` | Marks messages shown |
| `directory.list` | The `who` listing |
| `message.ask` / `message.tell` / `message.reply` | Send |
| `messages.fetch` | Unread messages, rendered, with an optional wait |
| `responder.take` | The next question for a responder |
| `daemon.status` | Version, device, roster, network, links and counts |
| `directory.refresh` | Re-reads the roster now |
| `devices.list` / `devices.revoke` | Your devices from spew |
| `auth.login` / `auth.logout` | spew sign-in (the daemon sends the page to open as an `auth.login.url` notification) |
| `daemon.shutdown` | Stops the daemon |

- Every method that acts as a session needs the 128-bit token `session.open` returned. A session can read and
  acknowledge only its own messages. Hooks are stateless, so `hook.event` names the session by its client and
  client session id instead, and can act only for the session in its payload.
- Errors are JSON-RPC errors with `data.reason` set to one of these:

| `data.reason` | Code |
|---|---|
| `not-found` | -32001 |
| `refused` | -32002 |
| `forbidden` | -32003 |
| `conflict` | -32004 |
| `timeout` | -32005 |
| `invalid` | -32006 |
| `unavailable` | -32007 |

Any process of your own account can use the socket. That is deliberate: the same processes can already read your
files. Section 13 of [design.md](design.md) covers this.
