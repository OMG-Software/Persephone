# Route every connection through the profile's SOCKS5 proxy

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for a SOCKS5 proxy
setting.

## Context and Problem Statement

The homeserver, and anyone watching this computer's network, sees its IP
address and which servers it talks to. A SOCKS5 proxy such as Tor's hides
both. Persephone made every connection directly and refused ambient
proxies (`no_proxy`), so that system settings could not redirect it. A
proxy must cover the sign-in itself and discovery, so it cannot be an
account setting, which exists only once signed in. Where does the setting
live, which connections use it, and what happens when it fails?

## Decision Drivers

* Every connection the core makes, including before sign-in.
* Names resolved by the proxy, not this computer's DNS.
* No silent fallback to a direct connection.
* No new crate; explicit dependency features only.

## Considered Options

* Honour the operating system's proxy settings.
* A setting of the profile, applied by the core's single HTTP client
  policy.

## Decision Outcome

Proposed option: "a setting of the profile, applied by the core".

* The setting is a host and port, kept as one record in the profile's
  encrypted crypto store; `proxy_setting` and `set_proxy_setting` work
  whenever the profile is unlocked, signed in or not. No user name or
  password is kept.
* `homeserver::http_client` takes the proxy and uses it as `socks5h`, so
  the proxy resolves the names. Ambient proxies stay off. Every client the
  core builds goes through it: the Matrix SDK client (sync, sending,
  account changes), `.well-known` discovery, and image downloads, which now
  use the SDK client's HTTP client instead of a separate one.
* If the proxy does not answer, the connection fails. If the record cannot
  be read, the core refuses to build a client or to discover a server
  rather than connect directly.
* A change applies to clients built afterwards; the page says it takes
  effect from Persephone's next start.
* The page offers the setting on the sign-in card (folded under
  "Connection") and in Preferences.
* reqwest's `socks` feature is enabled. In reqwest 0.13.5 it uses
  hyper-util's proxy connector, which reqwest already builds: no crate and
  no `Cargo.lock` entry changes.

### Positive Consequences

* The homeserver and the local network see the proxy, not this computer.
* The proxy covers sign-in and discovery as well as the signed-in session.

### Negative Consequences

* Links opened in the browser go through the browser's own settings, and
  the page says so.
* A misconfigured proxy stops all connections until it is fixed or
  removed.
* The proxy operator sees which homeservers Persephone connects to; TLS
  still protects the content.
* Tested with unit tests of the address rules and the client builder; no
  connection through a real proxy is recorded.

## Links

* Extends the connection policy of `crates/core/src/homeserver.rs` and
  [ADR-0027](0027-discover-the-homeserver-through-well-known-in-the-core.md).
