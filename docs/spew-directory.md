# The spew directory API

With the `spew` roster, a spew mesh daemon signs in to a spew server, registers its device key there, and reads
from it which devices may connect. This page is the contract for that API, for anyone running their own directory.
It is section 14 of [design.md](design.md), arranged as a reference.

The daemon only reads rosters and manages its own device. Messages and presence never pass through the directory;
they go between devices.

## Basics

- Base URL: `[spew] url` in settings, default `https://mcp.spew.lol`. It must be `https`; `http` is allowed only on
  `127.0.0.1` or `localhost`, for tests. Every endpoint in the OAuth metadata must follow the same rule.
- Every request and response body is JSON, except the OAuth token and revocation requests, which are form-encoded.
- The mesh API endpoints take `Authorization: Bearer <access token>`.
- Errors use this shape:

```json
{"error": {"code": "grant-has-device", "message": "This sign-in already has a device."}}
```

- These endpoints should never count as agent calls, and should have their own rate limits.

## Sign-in

OAuth 2.1 for a public client: PKCE with `S256` and a loopback redirect (RFC 8252).

### Discovery

`GET /.well-known/oauth-authorization-server` returns the authorization server metadata. The client reads
`authorization_endpoint`, `token_endpoint`, `registration_endpoint`, `revocation_endpoint`, `scopes_supported` and
`code_challenge_methods_supported`.

The client says "this spew doesn't support mesh sign-in yet" when discovery returns 404, `scopes_supported` lacks
`mesh`, `S256` isn't offered, or there is no `registration_endpoint` and no client id saved from an earlier
registration.

### Client registration

Dynamic client registration at `registration_endpoint`:

```json
{
  "client_name": "spew mesh",
  "redirect_uris": ["http://127.0.0.1/callback"],
  "token_endpoint_auth_method": "none",
  "grant_types": ["authorization_code", "refresh_token"]
}
```

The response's `client_id` is saved at once, so an abandoned sign-in reuses it next time.

### Authorization

The client opens the browser at `authorization_endpoint` with:

| Parameter | Value |
|---|---|
| `response_type` | `code` |
| `client_id` | the registered client id |
| `redirect_uri` | `http://127.0.0.1:<random port>/callback` |
| `code_challenge` | the S256 challenge |
| `code_challenge_method` | `S256` |
| `state` | a random value |
| `scope` | `mesh` |

The person signs in, picks the projects this device joins, and approves. The consent page should show the `mesh`
scope and the device name. The client sends no `resource` parameter.

The client's loopback listener binds `127.0.0.1` only, accepts exactly one valid callback, answers a request with a
wrong or missing `state` with 400 and keeps waiting, and gives up after 5 minutes.

### Token

`POST token_endpoint`, form-encoded:

- First token: `grant_type=authorization_code`, `code`, `redirect_uri`, `code_verifier`, `client_id`.
- Refresh: `grant_type=refresh_token`, `refresh_token`, `client_id`.

Rules the client enforces:

- The first token response must carry `token_type: "Bearer"` and `scope`, and the scope must be exactly `mesh` (as
  a set of space-separated values). Anything else, including `mesh` plus more, is a token that can do more than
  the mesh API needs: the client revokes it at once, stores nothing, and reports that this spew doesn't support
  mesh sign-in yet.
- A refresh response may leave `scope` out (unchanged). One that names any other scope is revoked and signs out.
- Refresh tokens should rotate. A refresh answered with 400 or 401 (such as a reuse error) signs the device out,
  and the person signs in again.

Each sign-in creates a new grant. A grant holds at most one device (see below), so starting over after a lost
device key works.

### Sign-out

`spew mesh logout` calls `DELETE /api/mesh/v1/devices/self`, then revokes the refresh token at
`revocation_endpoint` (RFC 7009).

## Who am I

`GET /api/mesh/v1/me`

```json
{
  "user": {"username": "sam", "name": "Sam"},
  "device": {"name": "sam-mac", "public_key": "1995f53bd1...", "created_at": 1760000000000},
  "projects": [{"key": "spew", "name": "spew"}]
}
```

`device` is the device bound to this grant, or `null` before registration. A 404 here also means the server
doesn't support mesh sign-in.

## Devices

A device is one daemon with one ed25519 key. Its public key, as 64 lowercase hex characters, is its endpoint id.

### Register (proof of possession)

1. `POST /api/mesh/v1/devices/challenge` with body `{}` returns a nonce, valid for 5 minutes and usable once:

   ```json
   {"nonce": "<base64url of 32 random bytes>", "expires_at": 1760000300000}
   ```

2. `POST /api/mesh/v1/devices` with:

   ```json
   {
     "public_key": "1995f53bd11964eb3557122683c5475b01ca4c8f0289045ca730b58a6a807695",
     "name": "sam-mac",
     "platform": "macos",
     "nonce": "<the nonce>",
     "signature": "<base64url ed25519 signature>"
   }
   ```

   The signature covers the UTF-8 bytes of these four lines, joined by `\n` with no trailing newline:

   ```text
   spew-mesh-device-v1
   https://mcp.spew.lol
   1995f53bd11964eb3557122683c5475b01ca4c8f0289045ca730b58a6a807695
   <the nonce>
   ```

   The second line is the base URL normalized: lowercase scheme and host, default port dropped, no trailing slash.
   The third is the lowercase hex public key. The client sends the signature as unpadded base64url.

   The response is the device: `{"name": "sam-mac", "public_key": "...", "created_at": <ms>}`.

- Registering the same key again for the same user is idempotent.
- A key registered to another user is refused.
- The server binds the device to the grant that registered it. Later calls with that grant's tokens (refreshes keep
  the grant) act for that device.
- A grant holds at most one device. Registering a different key under a grant that already has one is refused with
  409 and the code `grant-has-device`.

Device names are lowercase ASCII letters, digits, `.`, `_` and `-`, 1 to 48 characters. `platform` is a short
lowercase word such as `macos` or `linux`.

### Update

`PATCH /api/mesh/v1/devices/self` updates the calling device:

```json
{"relay": "https://relay.example.com/", "name": "sam-mbp"}
```

`relay` is the device's home relay URL, or `null`. `name` is optional. The daemon sends this when its home relay
changes, at most once a minute.

### Unregister

`DELETE /api/mesh/v1/devices/self` unregisters the calling device. The client treats 404 as already done.

### List

`GET /api/mesh/v1/devices` returns the caller's own devices:

```json
{
  "devices": [
    {"name": "sam-mac", "public_key": "...", "platform": "macos", "relay": null,
     "created_at": 1760000000000, "last_seen_at": 1760003600000, "self": true}
  ]
}
```

`self: true` marks the calling device.

### Revoke

`DELETE /api/mesh/v1/devices/<public_key>` revokes one of the caller's own devices, for a lost or stolen computer.
The server deletes that device's grant and changes the peers `version`, so every roster drops the device at its
next refresh.

## Peers

`GET /api/mesh/v1/peers` returns the roster. It supports `If-None-Match`, answers with an `ETag` header, and
returns 304 when nothing changed.

```json
{
  "version": "opaque",
  "ttl_secs": 60,
  "me": {
    "username": "sam", "display": "Sam",
    "devices": [{"endpoint_id": "...", "name": "sam-mac", "relay": null}]
  },
  "rooms": [
    {
      "id": "spew", "name": "spew", "kind": "project",
      "members": [
        {"username": "sam", "display": "Sam",
         "devices": [{"endpoint_id": "...", "name": "sam-mac", "relay": null}]},
        {"username": "maya", "display": "Maya",
         "devices": [{"endpoint_id": "...", "name": "maya-mbp", "relay": "https://relay.example.com/"}]}
      ]
    }
  ]
}
```

- `me.devices` lists all of the caller's registered, unrevoked devices, whatever projects were chosen at consent.
  They form the caller's private room, so a person's devices find each other with no shared project.
- `rooms` lists only the projects this device was allowed at consent, one room per project, with `id` set to the
  project key. Each lists only members of that project and only their registered, unrevoked devices.
- No emails and no internal ids.
- Removing a person from a project, revoking a device, or deleting a project changes `version`.
- `ttl_secs` must be 1 to 86400. The client refuses a roster outside that range and keeps its last good one.
- Person names and room ids are lowercase ASCII letters, digits, `.`, `_` and `-`, 1 to 32 characters, never
  `me`. The client drops an entry outside these rules and names it in `doctor`.

### How the client uses it

- It refreshes every `[spew] refresh_seconds` (default 60) with `If-None-Match`, and at most once every 10 seconds
  when an unknown endpoint id tries to connect.
- On failure it keeps the last good roster. Teammates' devices stay admitted only while it is younger than
  max(15 × `ttl_secs`, 900) seconds. After that only the caller's own devices stay, for `[spew]
  max_stale_seconds` (default 86400), and then only this device. Both windows count from the last 200 or 304.
- A 401 that a refresh can't fix signs the device out at once: teammates are dropped and only the caller's own
  devices from the last roster stay, as above.
