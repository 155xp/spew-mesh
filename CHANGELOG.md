# Changelog

All notable changes to spew mesh are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 - Unreleased

The first release.

### Fixed

- Expand Codex serve's secret-file deny rules at every directory depth, and check a deeply nested `.env` at
  startup so a depth-limited sandbox cannot silently expose secrets to a peer's question.
- Strip default-ignorable Unicode characters before escaping peer message boundaries, so invisible characters
  cannot disguise a message header or footer.
- Omit settings source lines from parse errors so a malformed relay-token setting cannot copy its credential
  into doctor output or daemon diagnostics.

### Added

- `spew mesh`, one binary with a daemon per computer that starts on demand.
- Agent sessions on one computer ask, tell and answer each other with no sign-in and no network.
- Peer-to-peer links between computers over iroh, admitted only by a roster: `local` (this computer only),
  `file` (a `roster.toml` you share) or `spew` (spew sign-in, with one room per spew project).
- Network modes `n0` (n0's public relays and address lookup), `own` (your relays only) and `local` (mDNS and
  roster addresses only).
- Presence: `spew mesh who` lists live sessions with person, device, client, repo and branch, spew card,
  activity, and busy or idle. Teammates see only the rooms they share.
- Five MCP tools for agents (`who`, `ask`, `tell`, `reply`, `messages`) and the same commands in the CLI.
- Addresses by session name, person, `person/name`, `person@device/name`, client kind, or spew card
  (`spew#42`).
- `ask` waits for the answer to that exact question and reports where the question is when the wait ends.
- Messages wait in the sender's outbox until the receiving computer is online, with receipts for delivered,
  shown, answered, refused and expired.
- `spew mesh setup` wires Claude Code (MCP server, hooks, skill), Codex (MCP server, hooks) and OpenCode (MCP
  server, plugin, skill), with backups, atomic edits and a manifest. `spew mesh uninstall` reverts exactly what
  setup wrote.
- Hooks show incoming messages at an agent's next step. They never block a stop and never start an idle agent.
- spew card capture: sessions advertise the cards they claim through spew's own tools.
- `spew mesh serve claude|codex|opencode` answers questions with headless runs, read-only by default, with
  per-person and hourly run limits, a timeout, and conversation memory for follow-ups.
- `spew mesh doctor` checks the setup, the daemon, the roster, the network and each agent, and says what to fix.
- spew sign-in (OAuth with PKCE and a loopback redirect) and device registration with proof of possession, for
  spew servers that support it. `spew mesh devices` lists and revokes your devices.
- Release binaries for Linux (x86_64, arm64; static) and macOS (Apple silicon, Intel), with SHA-256 checksums.

