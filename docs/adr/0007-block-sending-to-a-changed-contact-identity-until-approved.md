# Block sending to a changed contact identity until approved

* Status: accepted
* Deciders: project owner
* Date: 2026-10-23

## Context and Problem Statement

Under [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md),
Persephone shares room keys only with devices signed by their owner's current
cross-signing identity. The SDK remembers the first identity it sees for each
user ("pinning"). When a contact's identity later changes, the SDK still
shares keys with the new identity's devices unless this account had verified
the contact. A legitimate reset (lost devices and recovery key) and an attack
by someone who controls the homeserver both look like this. ADR-0005 left the
warning UI open. Persephone needs a rule for what happens to sending, and who
may accept a new identity.

## Decision Drivers

* A homeserver must not obtain keys for new messages by replacing a
  contact's identity without the user noticing.
* Only the user can tell a reset from an attack, by checking with the contact
  directly.
* A compromised web view must not be able to accept a new identity on the
  user's behalf (ADR-0004).
* Never send in plain text after an encrypted send is refused.

## Considered Options

* Unverified contact changes identity: warn and keep sending (as Element
  does), or block sending until the user approves.
* Verified contact changes identity (the SDK already refuses to send): allow
  only verifying again, or also withdrawing the verification.
* Who approves: the page, or the core's own window.

## Decision Outcome

Chosen options (owner decisions, 23 October 2026):

* **Block until approved.** Persephone refuses to send in an encrypted room
  while any member's identity changed since it was pinned, and nothing is
  sent. The page names the members and offers to accept the new identity.
  Accepting pins it; the refused messages can then be sent again.
* **Verified contacts: verify again or withdraw.** When a contact this
  account verified changes identity, the user can withdraw the verification,
  which pins the new identity, and can verify the new identity by comparing
  emoji. In this order only: a contact verification request is itself an
  encrypted message in the direct-message room, and the SDK refuses every
  encrypted send there while a verified member's identity is changed (a
  technical constraint found in the live runs on 23 October 2026, not a
  separate owner decision). An unverified contact whose identity changed can
  be verified instead of accepted.
* **The core's own window confirms.** The page only asks; the core shows a
  native confirmation naming the contact, with "No" as the default, and
  changes nothing without an explicit yes there.

### Positive Consequences

* A replaced identity gets no keys for what Persephone sends until the user
  has looked at it.
* A forged page request cannot accept a new identity.

### Negative Consequences

* Sending in a room stops after any member resets their encryption, until
  the user acts; in a large room this can happen often.
* After a verified contact changes identity, the new identity receives keys
  from the moment the verification is withdrawn until it is verified again.
* The check runs before each send. An identity that changes between the check
  and the SDK's key sharing still receives that one message's key.
* Messages from a contact's new identity are still shown: this decision
  covers sending only.
* A homeserver that hides identity changes from Persephone is not detected.

## Links

* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
* [MSC4153: exclude insecure devices](https://github.com/matrix-org/matrix-spec-proposals/pull/4153)
