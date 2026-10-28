# Discover the homeserver through .well-known in the core

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for homeserver
discovery through `.well-known`.

## Context and Problem Statement

People know their server by its name (`example.org`), while the client API
often lives elsewhere (`matrix.example.org`). Persephone accepted only a
full `https://` address. The SDK can follow `.well-known` during sign-in,
but that uses its own HTTP path and shows the user nothing. A
`.well-known` file can point to any host, so following it silently would
let whoever controls the name's web server choose where the password goes.
Who looks it up, and what does the user see?

## Decision Drivers

* Keep every connection on the client policy: TLS 1.3, no proxy, no
  redirects (security design; owner decision of 20 October 2026).
* Show the user the server found before any credential is sent.
* Bound what is read from an untrusted server.

## Considered Options

* Let the SDK follow `.well-known` at sign-in.
* Look it up in the core, confirm the server, and show it to the user first.

## Decision Outcome

Proposed option: "look it up in the core and show it first".

* The server picker accepts a server name or a full address. A name is
  sent with `discover_homeserver`; the core accepts only a host with an
  optional port.
* The core fetches `https://<name>/.well-known/matrix/client` with the
  client policy, at most 64 KiB within 10 seconds. A 404 means the name
  itself is tried; any other failure stops. The `m.homeserver.base_url`
  must pass the same check as a typed address (`https://`, a host, no
  path).
* The core confirms the server with `/_matrix/client/versions`; without a
  list of versions there is no Matrix server.
* The page shows the address found, and whether `.well-known` named it, and
  the user presses again to use it. Sign-in keeps
  `respect_login_well_known(false)`, so the login response cannot move the
  account to another server either.
* Discovery needs no session: it runs on its own small runtime, one at a
  time.

### Positive Consequences

* Users can type their server's name.
* Nothing is sent to a server the user has not seen named.

### Negative Consequences

* One more press for a name, by design.
* A homeserver under a path prefix (`https://example.org/matrix/`) is
  refused, as a typed one is.
* Tested with unit tests of the name check and the file and version
  parsing only; the HTTP requests are not exercised by an automated test.

## Links

* Applies the connection policy of `crates/core/src/homeserver.rs`.
