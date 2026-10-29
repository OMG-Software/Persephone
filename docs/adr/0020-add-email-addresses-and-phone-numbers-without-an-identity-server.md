# Add email addresses and phone numbers without an identity server

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-27

Technical Story: on 27 October 2026 the owner asked to list, add and remove
the account's email addresses and phone numbers (third-party identifiers,
3PIDs) in the settings.

## Context and Problem Statement

A homeserver uses 3PIDs for password reset and notices. Adding one is a
three-step flow: the homeserver sends a link or a code, the user proves they
received it, and the address is added with UIA. A homeserver can also bind
an address to an identity server, which makes the user findable by it. Which
parts may the page see, and where may confirmation codes go?

## Decision Drivers

* No password or confirmation code crosses to the UI process (ADR-0004).
* Addresses are personal data but not secrets; the user types them anyway.
* Do not bind addresses to an identity server or contact one: it would
  publish a lookup of the user's address.
* Treat homeserver answers, including a `submit_url`, as untrusted.
* A homeserver without support gets a clear, fixed message.

## Considered Options

* Everything in the core's windows, addresses included.
* Addresses in the page; codes and the password in the core's windows.
* Use an identity server for validation, as older clients did.

## Decision Outcome

Proposed option: "addresses in the page; codes and the password in the
core's windows", without any identity server.

* `three_pids` lists the addresses (projected like names, at most 50) and
  `m.3pid_changes` from the capabilities (on unless the homeserver says
  otherwise), and the address waiting to be added, if any.
* `request_email_token` and `request_phone_token` send the address, checked
  in the core first: an email address is printable ASCII with one `@` and a
  dotted domain; a phone number is a two-letter country code and digits. The
  core makes the client secret itself and keeps the session ID; only one
  address waits at a time, bound to the session generation.
* `add_three_pid` takes no argument. For a phone, the core asks for the code
  in its own window and posts it to the `submit_url` only when that URL has
  the homeserver's own scheme, host and port; without such a URL the code
  could be checked only by an identity server, so the medium is reported as
  not supported. Then the core asks for the password and adds the address
  with UIA, as in ADR-0019. A link not yet opened is `not_confirmed`, and
  the address keeps waiting.
* `remove_three_pid` names the medium and address; no `id_server` is sent.
* Refusals are fixed codes: `in_use`, `denied`, `not_confirmed`,
  `medium_not_supported` (also for `M_UNRECOGNIZED`, an endpoint the
  homeserver lacks), `changes_disabled`, `nothing_pending`, and the codes of
  ADR-0019.

### Positive Consequences

* Users can keep a recovery address on the account without leaving
  Persephone, and without becoming findable through an identity server.
* Each refusal, including a homeserver without support, has its own text.

### Negative Consequences

* Homeservers that delegate phone validation to an identity server
  elsewhere cannot add phone numbers through Persephone.
* The address the page sends is visible to the UI process, like any typed
  text.
* Linux has no native prompt yet, so adding fails as `unsupported` there.
* Tested against a mock homeserver only, not yet against Merovingian or
  Synapse.

## Links

* Keeps [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Uses the authentication of [ADR-0019](0019-change-the-password-with-secrets-entered-in-the-core.md)
