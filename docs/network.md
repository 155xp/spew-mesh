# Network

This page covers how spew mesh daemons find and reach each other, who may connect, and how to check it. All
settings live in `~/.spew/mesh/settings.toml` (or `$SPEW_MESH_DIR/settings.toml`). The file is optional; without
it every setting has its default. A file that doesn't parse, an unknown key, or a value out of range stops the
daemon with an error naming it, and `spew mesh doctor` shows the same error.

## No network until it is needed

A daemon opens no network endpoint while its roster lists no other device. With the default `local` roster, a
computer on its own uses no network and publishes nothing; your sessions on it still talk to each other through
the daemon. The endpoint comes up when the roster first lists another device, and closes when it lists none again.

`spew mesh daemon status` and `doctor` show which state the daemon is in: `network: off`, `local`, `n0` or `own`.

## Modes

`[network] mode` picks how devices find each other:

| Mode | Relays | Address lookup | Use it for |
|---|---|---|---|
| `"n0"` (default) | n0's public relays | n0's DNS address lookup, mDNS, roster hints | Most people |
| `"own"` | Only the relays you list | mDNS and roster hints | Teams that run their own relay |
| `"local"` | None | mDNS and roster hints | LAN-only or air-gapped teams |

Connections between devices are QUIC with TLS, end to end, through iroh. Two devices connect directly whenever
they can. A relay forwards encrypted traffic when they can't, and helps them find a direct path.

### n0 (default)

```toml
[network]
mode = "n0"
```

n0 runs public relays and a DNS address lookup service for iroh. They are free, rate limited, and have no uptime
guarantee. Most connections end up direct, so they carry little traffic, but while they are slow or down, devices
that need them to find or reach each other can't connect.

The daemon publishes a record to n0's DNS that maps its endpoint id to its home relay URL. That record is public.
Nothing else goes there: no names, no presence, no sessions.

`doctor` shows the home relay the daemon is using.

### own

```toml
[network]
mode = "own"
relays = ["https://relay.example.com"]
relay_token = "a long random token"    # only if your relays require one
```

The daemon uses exactly the relays you list and no n0 service at all. `relay_token`, when set, is sent to your
relays as a bearer token. It is never logged.

To run a relay, use iroh's relay server, `iroh-relay` (`cargo install iroh-relay --features server`). iroh's
documentation covers its configuration, TLS and access control; a shared token there pairs with `relay_token`
here. Give every device in your roster the same `relays` list.

### local

```toml
[network]
mode = "local"
```

No relays and no n0 services. Devices find each other through mDNS on the local network, or through the `addrs`
you write in the roster file. Use it for a LAN-only team, for an air-gapped network, or for testing.

## mDNS

In every mode, the daemon announces itself and looks for peers on the local network with mDNS, under the service
name `spewmesh`. Only devices your roster admits can connect, whatever mDNS finds.

Turn mDNS off with:

```toml
[network]
mdns = false
```

or for one run with the environment variable `SPEW_MESH_LAN=0`. Tests always run with `SPEW_MESH_LAN=0`.

## Bind address and port

```toml
[network]
bind = "0.0.0.0"   # an IPv4 or IPv6 address
port = 0           # 0 picks any free port
```

With the defaults, the daemon binds any free port on every IPv4 and IPv6 address. Set `port` to a fixed number
when a firewall needs one. Set `bind` to one address to listen only there. With `bind = "0.0.0.0"` the daemon also
binds `[::]` on the same port, and carries on without IPv6 when that fails.

`spew mesh daemon status` lists the sockets it bound (`bound: ...`).

## Rosters

A roster decides who may connect. A device may connect only if it is one of your own devices, or it appears in a
room that also has you in it. Everyone else is closed right after the TLS handshake, which proves the device's key.

`[roster] kind` picks the provider:

| Kind | Who is in it |
|---|---|
| `"local"` (default) | Only this device. Nothing connects from the network. |
| `"file"` | The people, devices and rooms in a roster file you write. |
| `"spew"` | Your own devices and the people in the spew projects you chose, after `spew mesh login`. |

### The roster file

For teams without spew, or LAN-only teams. Each person runs `spew mesh identity` and shares the endpoint id it
prints:

```console
$ spew mesh identity
sam-mac
endpoint id: 1995f53bd11964eb3557122683c5475b01ca4c8f0289045ca730b58a6a807695
short id: 1995f53bd1
```

Write one roster file and give everyone a copy. Its default place is `~/.spew/mesh/roster.toml`:

```toml
[[person]]
name = "sam"
display = "Sam"
devices = [
  { id = "1995f53bd11964eb3557122683c5475b01ca4c8f0289045ca730b58a6a807695", name = "sam-mac" },
  { id = "<sam's other endpoint id>", name = "sam-linux", addrs = ["192.168.1.20:7461"] },
]

[[person]]
name = "maya"
devices = [
  { id = "33a9c8693da2f7edd5125e9d3b010baca09084ccbcbe7216b01a49147f39cc1b", name = "maya-mbp" },
]

[[person]]
name = "lee"
devices = [
  { id = "<lee's endpoint id>", name = "lee-linux", relay = "https://relay.example.com" },
]

[[room]]
name = "web"
people = ["sam", "maya", "lee"]
```

- `[[person]]`: `name` is lowercase ASCII letters, digits, `.`, `_` and `-`, 1 to 32 characters, and never `me`.
  `display` is optional, one line of at most 64 characters.
- `devices`: `id` is the 64-character endpoint id. `name` uses the same characters as person names, 1 to 48. A
  device can belong to only one person, and one person's device names must differ.
- `relay` and `addrs` are optional dialing hints: the device's home relay, and IP addresses with ports where it can
  be reached. With them, a device can be dialed without any address lookup. A fixed `port` in that device's
  settings makes `addrs` stable.
- `[[room]]`: `name` uses the person-name characters. `people` lists people from the file, at least one.
- Your own devices form your private room automatically; you don't list them in a room.

Then, on each computer, point the settings at it:

```toml
username = "sam"

[roster]
kind = "file"
file = "~/.spew/mesh/roster.toml"   # the default; a relative path is inside the spew mesh home
```

- `username` must name a person in the file whose devices include this device. If it doesn't, or the file can't be
  read or breaks a rule, the daemon runs with the local roster, and `doctor` says why.
- The daemon notices changes to the file within 5 seconds. Removing a device or a person closes its connections at
  once and fails the mail queued for it.

### spew

```toml
[roster]
kind = "spew"

[spew]
url = "https://mcp.spew.lol"   # the default
refresh_seconds = 60
max_stale_seconds = 86400
```

`spew mesh login` (also run by `setup`) signs in through your browser and registers this device's key with spew.
At consent you choose which projects this device joins. Each project becomes a room. `spew mesh devices` lists
your devices, and `spew mesh devices revoke <name>` removes one you lost.

With the default `kind = "local"`, signing in switches to the spew roster on its own. `kind = "spew"` does the
same, and also makes `doctor` say so while you aren't signed in. `kind = "file"` uses the file even when you are
signed in.

- The daemon refreshes the roster every `refresh_seconds` (10 to 86400), and sooner when an unknown device tries
  to connect.
- When spew can't be reached, the daemon keeps the last roster. Teammates stay admitted only while it is fresh
  enough (at least 15 minutes, longer when spew says so). Your own devices stay for `max_stale_seconds` (0 to
  2592000). After that, only this device.
- If spew rejects the sign-in, teammates are dropped at once, local work carries on, and `doctor` says to run
  `spew mesh login`.
- `url` must be `https` (`http` only on `127.0.0.1` or `localhost`, for tests).

[spew-directory.md](spew-directory.md) describes the API, for anyone running their own directory.

## Sessions and rooms

A session's room decides which teammates see it. It comes from, in order: the `SPEW_PROJECT` variable in the
agent's environment, the `project` key of the nearest `.spew.json` above the session's working folder, and
`[roster] default_room`. With none of them, the session is private: only your own devices see it, and your own
`who` lists it under "private: no .spew.json". A `.spew.json` that can't be read or has no valid `project` also
leaves the session private.

```toml
[roster]
default_room = "web"     # room for sessions whose repo has no .spew.json

[presence]
branch = true            # send the branch to teammates
place = true             # send the folder name to teammates
```

Teammates get a session's branch and folder name only when its room came from `.spew.json`, and only while these
are `true`. Your own devices always see everything.

## Checking it

`spew mesh doctor` prints the version, paths, the device, the roster and network state, links, and the agents,
then a `fix:` line for each problem. `spew mesh daemon status` prints the daemon part:

```console
$ spew mesh daemon status
daemon: 0.1.0 (pid 4182, up 2h)
home: /Users/sam/.spew/mesh
socket: /Users/sam/.spew/mesh/run/ipc.sock
device: sam-mac (1995f53bd1) on macos
endpoint id: 1995f53bd11964eb3557122683c5475b01ca4c8f0289045ca730b58a6a807695
roster: file, roster file /Users/sam/.spew/mesh/roster.toml, version file-4b9f8dd
rooms: web
admitted devices: 3
network: own, relay https://relay.example.com/
  link 33a9c8693d: direct, 4 ms
bound: 0.0.0.0:51820, [::]:51820
sessions: 2, unread: 0, queued: 0, online devices: 1
```

What the lines mean:

- `roster:` the provider in use, then its state. `(settings say file)` after the provider means the configured
  roster couldn't be used; the `fix:` line below it says why.
- `admitted devices:` how many other devices may connect.
- `network:` the mode, or `off` while no other device is admitted, with the home relay in `n0` and `own` modes.
- `link <short id>:` one line per connected device: the path (`direct` or `relay`) and the round-trip time.
- `queued:` messages in the outbox, waiting for their device to come online.

`spew mesh who --offline --links` shows each device by name, online or last seen, with its path and round-trip
time:

```console
$ spew mesh who --offline --links
...
devices
  sam@sam-mac  this device
  sam@sam-linux  offline, last seen 2d ago
  maya@maya-mbp  online  direct  4 ms
  lee@lee-linux  offline, last seen 3h ago
```

## Environment

| Variable | Effect |
|---|---|
| `SPEW_MESH_DIR` | The spew mesh home, instead of `~/.spew/mesh`. `--home DIR` wins over it. |
| `SPEW_MESH_LAN=0` | Turns mDNS off. |
| `SPEW_MESH_LOG` | The log filter, such as `debug` or `spew_mesh=debug`. The log is `~/.spew/mesh/logs/mesh.log`. |
| `SPEW_MESH_CLIENT` | The client name of an MCP server, set by setup in agent configs. |
