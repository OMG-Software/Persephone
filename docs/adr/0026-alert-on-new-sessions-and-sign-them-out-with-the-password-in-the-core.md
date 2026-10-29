# Alert on new sessions, and sign them out with the password in the core

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for an alert when a new
session signs in to the account, and for removing sessions.

## Context and Problem Statement

A session signed in with a stolen password can read new messages from
rooms whose members share keys with it, and Persephone listed sessions but
neither drew attention to a new one nor could sign one out. Signing out
another device needs user-interactive authentication, normally the account
password, which must not cross to the web view (ADR-0004). How does
Persephone notice a new session, and how does it sign one out?

## Decision Drivers

* A new session must be hard to miss.
* A fresh sign-in on this computer must not raise an alert by itself.
* The password stays in the core.
* No extra requests to the homeserver just to watch for sessions.

## Considered Options

* Compare the homeserver's device list (`/devices`) on a timer.
* Compare the devices in this computer's crypto store, which the sync keeps
  up to date, with a list of devices the user has been shown.

## Decision Outcome

Proposed option: "compare the crypto store with a list of devices shown".

* The core keeps the device IDs the user has been shown in the encrypted
  crypto store (at most 200, oldest dropped first). The first time there is
  no list, it records the account's current devices and reports none.
* `new_sessions` reports up to ten of the account's devices, other than
  this one, that are not in the list, with their names projected. The page
  asks when signed in, on each security change and every minute; it reads
  only the local store.
* The alert sits above the room ("A new session signed in to your
  account"), with "Sign it out" for each and "It was me", which records
  them with `acknowledge_sessions`. The core adds only IDs of the account's
  current devices, so the page cannot fill the list with anything else.
* `remove_device` signs out another device: the core asks for the account
  password in its own window, then sends the deletion, adding the password
  only if the homeserver asks (as for a password change, ADR-0019). This
  device is refused; signing it out is `sign_out`. The Sessions page offers
  "Sign out" on every other session too.

### Positive Consequences

* A session signed in with a stolen password shows within a minute of the
  sync learning about it, and can be signed out from here.
* No password crosses to the web view, and watching costs no requests.

### Negative Consequences

* A session the sync has not yet learned about is not reported until it is.
* "It was me" is the user's word; Persephone cannot tell.
* Signing out a session does not change the password; the alert says to.
* Tested with unit, protocol and IPC tests only; no live run against
  Merovingian or Synapse is recorded.

## Links

* Follows [ADR-0019](0019-change-the-password-with-secrets-entered-in-the-core.md) for secrets entered in the core.
