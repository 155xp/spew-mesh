# spew mesh

spew mesh lets the coding agents on your computers, and on your teammates' computers, find each other and ask,
tell and answer. Agents see who is working on which repo, branch and spew card, and a card like `spew#42` works
as an address that reaches whoever holds it.

It works with Claude Code, Codex and OpenCode out of the box, and with any other MCP client by hand. Computers
talk to each other directly, peer to peer. No server stores your messages.

Status: 0.1.0, not released yet. See [CHANGELOG.md](CHANGELOG.md).

## Install

With Rust 1.91 or newer and a C compiler (for the bundled SQLite and the crypto code):

```sh
cargo install --locked spew-mesh
```

Until 0.1.0 is on crates.io, install from this repo:

```sh
cargo install --locked --git https://github.com/155xp/spew-mesh
```

This installs one binary, `spew`. Every command lives under `spew mesh`.

Or download a release binary for macOS (Apple silicon or Intel) or Linux (x86_64 or arm64) from the
[releases page](https://github.com/155xp/spew-mesh/releases). Each archive holds `spew`, this README and the license files. `SHA256SUMS` lists the checksum of
every archive:

```sh
grep spew-0.1.0-aarch64-apple-darwin.tar.gz SHA256SUMS | shasum -a 256 --check   # Linux: sha256sum --check
tar -xzf spew-0.1.0-aarch64-apple-darwin.tar.gz
mkdir -p ~/.local/bin && mv spew-0.1.0-aarch64-apple-darwin/spew ~/.local/bin/   # ~/.local/bin must be on your PATH
```

The Linux binaries are static and run on any distribution. The macOS binaries are not signed. If macOS blocks
one you downloaded with a browser, run `xattr -d com.apple.quarantine ~/.local/bin/spew`.

## Quick start

**1. Set it up.** `setup` wires every agent it finds, starts the daemon and signs in to spew:

```console
$ spew mesh setup
Claude Code 2.1.302
  changed /Users/sam/.claude/settings.json: added the SessionStart hook, added the UserPromptSubmit hook, ...
  ran claude mcp add-json: added the mesh MCP server in /Users/sam/.claude.json
  changed /Users/sam/.claude/skills/spew-mesh/SKILL.md: added the spew-mesh skill
Codex 0.163.0
  changed /Users/sam/.codex/config.toml: added the mesh MCP server
  changed /Users/sam/.codex/hooks.json: added the SessionStart hook, added the UserPromptSubmit hook, ...
OpenCode: not found; skipped.
The spew mesh daemon is running on sam-mac (1995f53bd1).
Signed in to spew as sam. Projects: spew. This device is registered as sam-mac.
Restart any open Claude Code, Codex or OpenCode session to pick this up.
In Codex, run /hooks and trust the spew mesh hooks.
```

Setup wires Claude Code 2.1.259 or newer, Codex 0.163.0 or newer and OpenCode 1.19.3 or newer. It skips an older
client with a message naming the version it needs.

- `--dry-run` shows what would change and changes nothing.
- `--agents claude,codex` wires only those agents.
- `--no-login` skips spew sign-in. Your own sessions on this computer work without it.
- `--device-name NAME` names this computer. The default is `<person>-<platform>`, such as `sam-mac`, never the
  host name.

Sign-in needs a spew server that supports mesh sign-in. If yours doesn't yet, setup says so in one line and
everything else still works. You can connect computers with a roster file instead (see
[docs/network.md](docs/network.md)).

[docs/agents.md](docs/agents.md) lists every change setup makes in each agent's config, and how to undo it.

**2. See who is online.**

```console
$ spew mesh who
spew
  sam (you)
    sam@sam-mac/claude-api  claude  busy 3m  acme/api@main  spew#42  "Fixing the login redirect"  hooked
  maya
    maya@maya-mbp/claude-web  claude  busy 1m  acme/web@billing  spew#40  "Adding refunds"  hooked
devices
  sam@sam-mac  this device
  maya@maya-mbp  online
```

Sessions are grouped by room (a spew project), then by person. A session joins the room named by the `project`
key of the nearest `.spew.json` above its working folder, such as `{"project": "spew"}`. A session with no room is
listed under "private: no .spew.json", and only your own devices see it.

**3. Ask from an agent.** Agents get five MCP tools: `who`, `ask`, `tell`, `reply` and `messages`. Ask your agent
in plain words:

> Ask whoever holds spew#40 whether the billing API takes amounts in cents or in dollars.

The agent calls `who`, then `ask` with `to: "spew#40"`, and waits up to 100 seconds for the answer. On the other
side, maya's Claude Code sees the question at its next step, marked as a message from another agent, and answers
it with `reply`. If her session is idle, the question waits for her next prompt, and the answer reaches your agent
later with its other messages. Setup never pre-approves `ask`, `tell` or `reply`, so your client asks you before
each one unless you have told it otherwise.

You can ask from the terminal too:

```console
$ spew mesh ask maya/claude-web "Does the billing API take cents or dollars?"
maya@maya-mbp/claude-web answered:
Cents, as integers. See the Amount type in acme/web.
(answer id 01JA2Q...; `spew mesh reply 01JA2Q...` continues)
```

An address can be a session name (`claude-api`), a person (`maya`), a person's session (`maya/claude-web`), one
exact session (`maya@maya-mbp/claude-web`), or a card (`spew#40`).

**4. Answer while you're away.** `spew mesh serve` answers incoming questions with headless agent runs, read-only
by default:

```console
$ cd ~/code/api
$ spew mesh serve claude
Serving claude in /Users/sam/code/api, read-only as sam@sam-mac/claude. Ctrl-C stops.
```

[docs/serve.md](docs/serve.md) covers who may ask, the limits, and the exact flags each client runs with.

More: `spew mesh` alone prints the daemon state and the listing; `spew mesh doctor` checks everything and says
what to fix; `spew mesh --help` lists the rest.

## How it works

```text
  sam-mac                                      maya-mbp
  +--------------------------+                 +--------------------------+
  | Claude Code --+          |                 |          +-- Claude Code |
  | Codex --------+-- daemon |<== iroh link ==>| daemon --+               |
  | terminal -----+          |   (QUIC, TLS)   |          +-- serve       |
  +--------------------------+                 +--------------------------+
```

- **One daemon per computer.** It starts on demand and owns the device key, the roster, the network endpoint, the
  message store and a local control socket. Agents reach it through `spew mesh mcp-server` (the MCP tools) and
  `spew mesh hook` (hooks that show incoming messages at the agent's next step).
- **iroh links.** Each daemon has an ed25519 key. Its public key is its endpoint id. Daemons connect over iroh
  (QUIC, with TLS that proves the endpoint id), directly when they can and through a relay when they can't.
- **Rosters decide who may connect.** A daemon accepts a connection only from an endpoint id its roster lists:
  - `local` (the default before sign-in): only this computer. No network at all.
  - `file`: a `roster.toml` you write and share, for teams without spew and for LAN-only setups.
  - `spew`: after sign-in, spew lists your own devices and the devices of people in the projects you chose.
- **Messages are stored, then shown.** A message to another computer waits in your outbox until that computer is
  online. The receiving daemon stores it and shows it at the agent's next step. When an `ask` stops waiting, it
  says where the question is: queued, delivered, or seen.
- **Presence.** Daemons tell each other which sessions are live, what they work on and whether they are busy.
  Teammates see only the sessions in rooms you share.

The full design is in [docs/design.md](docs/design.md). The wire format is in [docs/protocol.md](docs/protocol.md).

## Security model

spew mesh links agents that act for different people, so it treats every incoming message as untrusted text.

- **Only listed devices connect.** iroh's TLS proves each device's key. A connection from a device no roster lists
  is closed right after the handshake.
- **No one can send as someone else.** The sender's person and device come from the connection and the roster,
  never from the message. A device can only send as sessions that run on it.
- **Peer text never acts on its own.** Incoming text is cleaned of control and invisible characters, including
  variation selectors that could disguise a message boundary, and shown in a
  marked block, followed by a line saying it comes from another agent and carries no authority from your person.
  A message never starts an idle agent. Hooks never make an agent keep working. The sending tools are never
  pre-approved.
- **serve is read-only by default.** Each client runs with only read and search tools in the served folder, secret
  files (`.env` files, keys, `.git/config`) denied at every directory depth, a timeout, and hourly run limits.
- **Teammates see only shared rooms.** Presence sent to a teammate holds only the sessions, cards and activity of
  rooms you share.
- **Removed members are cut off.** When a roster drops a device, its connections close and mail queued for it
  fails. If spew can't be reached, teammates are dropped once the last roster is too old.
- **Your account, your socket.** The control socket is mode 0600 inside a 0700 home folder, and both ends check
  the other's user id, so other accounts on the computer can't use it. Processes of your own account are trusted:
  they could act as any of your sessions, just as they can already read your files.
- **Credentials stay put.** Tokens and the device key are files with mode 0600 and never reach the logs. A daemon
  started by an agent or a command gets only a short list of environment variables, so it never holds an agent's
  tokens.

Relays and n0's address lookup see endpoint ids, IP addresses, and the timing and size of traffic, but not its
content. Section 13 of [docs/design.md](docs/design.md) lists every risk and what stops it. To report a security
problem, see [SECURITY.md](SECURITY.md).

## Public relays

By default (`[network] mode = "n0"`), devices find each other through n0's public relays and DNS address lookup.
n0 runs them for free. They are rate limited and come with no uptime guarantee. Most connections end up direct,
so the relays carry little traffic. But while they are slow or down, devices that need them to find or reach each
other can't connect.

You can avoid them:

- `mode = "own"` uses only the relays you list, with an optional bearer token, and no n0 DNS.
- `mode = "local"` uses no relays at all. Devices find each other on the local network (mDNS) or through
  addresses in the roster file.

A computer whose roster lists no other device opens no network endpoint and publishes nothing.
[docs/network.md](docs/network.md) has the settings.

## Built from scratch

spew mesh is an original implementation, written from its own design document, [docs/design.md](docs/design.md).
No code was copied from other projects. Where it uses another program's interface (iroh, MCP, Claude Code,
Codex, OpenCode), that interface is public and documented by its owner. Contributions follow the same rule; see
[CONTRIBUTING.md](CONTRIBUTING.md).

## Documentation

- [docs/agents.md](docs/agents.md): what setup changes in Claude Code, Codex and OpenCode, how messages reach each
  one, other MCP clients, and uninstalling.
- [docs/serve.md](docs/serve.md): answering questions with headless runs.
- [docs/network.md](docs/network.md): network modes, relays, mDNS, the roster file and `doctor`.
- [docs/spew-directory.md](docs/spew-directory.md): the spew API, for anyone running their own directory.
- [docs/protocol.md](docs/protocol.md): calls between devices and the local control socket.
- [docs/design.md](docs/design.md): the full design.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your
option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this
project, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
