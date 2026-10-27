# Register accounts with secrets entered in the core

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-27

Technical Story: on 27 October 2026 the owner asked for a register option on
the sign-in page once a homeserver is chosen: a desired username, a password
and a registration token when required, with clear status (such as a taken
username) shown back.

## Context and Problem Statement

Persephone could only sign in to an existing account. ADR-0004 keeps
passwords and other secrets out of the web view: the core asks for them in
its own windows. Registration needs a username, a password and sometimes a
registration token, and homeservers gate it with user-interactive
authentication (UIA) flows. How can the page offer registration and report
why it failed without any secret crossing to it?

## Decision Drivers

* No password or token crosses to the UI process (ADR-0004).
* A taken or invalid name is reported before the user types a password.
* Use the SDK's registration; follow only standard UIA steps.
* Failure reasons cross as fixed codes, never server text.

## Considered Options

* Every field in the page, sent to the core.
* Every field in the core's windows.
* The username in the page; the password and token in the core's windows.

## Decision Outcome

Proposed option: "the username in the page; the password and token in the
core's windows", because the username is not secret and can be checked and
corrected in the page, while the secrets stay in the core.

* `register` takes only the homeserver URL and a username: a Matrix localpart
  of lowercase letters, digits and `._=-/+`, checked by the page and again by
  the core before anything is sent.
* The core asks `/register/available` first. A taken, invalid or reserved
  name is reported without any prompt.
* The core then asks for the password twice, until both entries match, and
  registers. It follows the shortest flow made only of the registration-token
  and dummy steps, and asks for the token in its own window only when that
  flow needs it. A flow needing an email address, a captcha or accepting
  terms is `unsupported_registration`.
* On success the new session is saved encrypted and started like a sign-in.
* The status uses the sign-in status with new codes: `username_taken`,
  `invalid_username`, `username_reserved`, `registration_disabled`,
  `token_rejected`, `weak_password`, `unsupported_registration` and
  `rate_limited`.
* The sign-in page keeps the chosen server, mode and username across the
  session changes that registration causes, so a refused name can be edited.

### Positive Consequences

* New users can create an account from Persephone on homeservers with open
  or token-gated registration.
* The page learns no secret, and each refusal has a clear, fixed message.

### Negative Consequences

* Homeservers that require an email address, a captcha or accepting terms
  are not supported; the user registers in the homeserver's web page.
* The availability check tells the homeserver the name being considered
  before the user commits.
* Linux has no native prompt yet, so registration there fails as
  `unsupported`, like sign-in.
* Tested against a mock homeserver only, not yet against Merovingian or
  Synapse.

## Links

* Keeps [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
