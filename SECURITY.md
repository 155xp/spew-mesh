# Security policy

spew mesh connects agents that act for different people, so we take security reports seriously.

## Reporting a problem

Please report security problems privately, through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Don't open a public issue, pull request or
discussion for a security problem.

Include what you can:

- what an attacker can do, and from where (another device on the mesh, a teammate, a local process, a message
  body, a config file);
- the steps or a test that shows it;
- the spew mesh version (`spew --version`), the operating system, and the agent client and version, if one is
  involved.

We aim to acknowledge a report within a week, and to agree with you on a fix and a disclosure date. We credit
reporters in the release notes unless you'd rather we didn't.

## Supported versions

Only the latest release gets security fixes.

## In scope

Anything that breaks a promise in the security model (section 13 of [docs/design.md](docs/design.md) and the
README). For example:

- a device that no roster admits connecting, or a removed device staying connected after a roster refresh;
- a device sending as another device's sessions, as another person, or as the daemon's own notices;
- peer text that escapes its message block, forges a header, or survives sanitizing with control or invisible
  characters;
- a teammate seeing sessions, cards, activity, branches or folder names outside the rooms you share;
- another account on the same computer using the control socket or reading spew mesh's files;
- tokens, the device key or `Authorization` headers reaching a log, a command line or another process;
- `serve` reading, writing or reaching the network beyond what [docs/serve.md](docs/serve.md) allows, including
  through a repo's own config files;
- `setup` or `uninstall` damaging an agent config, or changing anything it didn't record;
- the spew sign-in accepting a token with more than the `mesh` scope, or a callback with the wrong `state`;
- floods that get past the rate limits, frame limits or conversation guard and take a daemon down.

## Out of scope

- Processes of your own account acting as one of your sessions. spew mesh trusts them by design, since they can
  already read your files.
- An agent choosing to follow a teammate's request. Messages are marked as coming from another agent, with no
  authority from your person, but the model decides what to do with them. Reports that show a way around the
  marking, the sanitizing or a limit are in scope.
- Problems in Claude Code, Codex, OpenCode, iroh or n0's public relays themselves. Please report those to their
  owners. If spew mesh uses one of them unsafely, that is in scope.
- What relays and n0's address lookup can see by design: endpoint ids, IP addresses, and the timing and size of
  traffic.
