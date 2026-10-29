# Change the password with secrets entered in the core

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-27

Technical Story: on 27 October 2026 the owner asked for a "change password"
setting, with a choice to sign out the account's other sessions.

## Context and Problem Statement

Persephone could not change an account's password. Changing it needs the
current password (for user-interactive authentication, UIA) and the new one.
ADR-0004 keeps passwords out of the web view, and ADR-0018 already asks for a
new account's password in the core's own windows. How can the settings page
offer a password change, with the choice the specification gives about other
devices, without any password crossing to it?

## Decision Drivers

* No password crosses to the UI process (ADR-0004).
* Use the SDK's request types and standard UIA; no server-specific path.
* Keep the specification's default: `logout_devices` is true.
* Report refusals as fixed codes, never server text.

## Considered Options

* Password fields in the page, sent to the core.
* Every password in the core's windows; the page sends only the choice.
* Send users to the homeserver's own web page.

## Decision Outcome

Proposed option: "every password in the core's windows; the page sends only
the choice", because it keeps the ADR-0004 boundary and matches sign-in,
encryption set-up and registration.

* `change_password` takes one argument, `sign_out_others`. The page shows it
  as "Sign out my other sessions", on by default.
* The core asks for the current password, then the new one twice until both
  match, in its own windows and with no lease held. Then it sends
  `POST /account/password` with `logout_devices` as chosen, follows a UIA
  flow made of `m.login.password` alone, and sends the password once. A
  second UIA answer (Matrix v1.19 adds the errcode of the failed stage) is
  `invalid_password`; any other flow is `unsupported_auth`.
* The reply is an `AccountChange` code: `done`, `cancelled`, `unsupported`,
  `invalid_password`, `weak_password`, `rate_limited`, `unreachable` or
  `failed`.
* One account change runs at a time; the prompt holds a long-call slot, so a
  profile switch waits for it, as for the image picker.

### Positive Consequences

* The password can be changed from Persephone; the page learns no secret.
* Signing out other sessions after a suspected compromise is one choice.

### Negative Consequences

* The SDK copies the passwords into its request; those copies are outside
  the zeroing guarantee, as at sign-in.
* Homeservers that require another UIA step (for example single sign-on) are
  not supported here.
* Linux has no native prompt yet, so the change fails as `unsupported`.
* Tested against a mock homeserver only, not yet against Merovingian or
  Synapse.

## Links

* Keeps [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Follows [ADR-0018](0018-register-accounts-with-secrets-in-the-core.md)
