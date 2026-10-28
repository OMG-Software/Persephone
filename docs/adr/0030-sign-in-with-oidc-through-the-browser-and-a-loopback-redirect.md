# Sign in with OIDC through the browser and a loopback redirect

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for OIDC sign-in.

## Context and Problem Statement

Homeservers that use Matrix's OAuth 2.0 API (next-generation auth, through
a provider such as the Matrix Authentication Service) do not accept a
password from the client; the user signs in on the provider's page.
Persephone signed in only with a password typed into the core's own
window. A provider's page needs a browser, and its answer has to come back
to the core without the web view seeing it (ADR-0004). How does Persephone
show the page, receive the answer, and keep the session?

## Decision Drivers

* The password and the authorization code never reach the web view.
* No embedded browser in the key-holding process, and no new dependency.
* The user sees which provider they are sent to.
* A lock must not wait on an open sign-in.

## Considered Options

* An embedded web view for the provider's page.
* The system browser with a private-use URI scheme registered with the OS.
* The system browser with a loopback redirect (RFC 8252).

## Decision Outcome

Proposed option: "the system browser with a loopback redirect".

* "Sign in with single sign-on" sends only the homeserver's address. The
  core reads the provider's metadata (none: `oidc_unavailable`), binds
  `127.0.0.1` on a port the system chooses, and registers Persephone
  dynamically as a native client whose only redirect is
  `http://127.0.0.1:<port>/callback`, with the project page
  `https://omg-software.github.io/Persephone/` as its URI.
* It asks in its own window, naming the homeserver and the provider's host
  (https only, ASCII only), before it opens the provider's page with the
  same opener as links (ADR-0024). "No" aborts the login.
* It waits for one `GET /callback?…` on the loopback address, at most
  16 KiB and five minutes, answering other requests with an error; the SDK
  checks the state it sent. The wait checks every half second whether a
  lock began, and gives up if so, so the lease it holds never keeps a lock
  waiting long.
* The SDK finishes the login. The session is saved in the encrypted store
  as the client ID and the user session; records from before OAuth (a
  password session) still read, and one with both or neither is refused.
  The client handles refresh tokens, and the sync loop saves the session
  again whenever the access token changed. Sign-out uses the SDK's
  auth-agnostic logout, which revokes an OAuth token too.
* `tokio`'s `net` and `io-util` features are added for the loopback
  listener; the SDK's OAuth support needs no feature, and its crates were
  already in `Cargo.lock`.

### Positive Consequences

* Accounts on OAuth-only homeservers can sign in.
* Neither the password nor the code passes through the web view.

### Negative Consequences

* A local program can race to answer the loopback port with a forged
  redirect; the state check and the provider's PKCE limit what it gains.
* Five minutes to finish; a slow user starts again.
* Account registration through OAuth is not offered.
* The provider learns the project page as Persephone's client URI.
* Tested with unit tests of the redirect listener, the question and the
  saved records; no sign-in against a real OAuth provider is recorded.

## Links

* Uses the opener of [ADR-0024](0024-open-links-only-after-the-core-asks-in-its-own-window.md).
