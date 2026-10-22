# Share room keys only with cross-signed devices

* Status: accepted
* Deciders: project owner
* Date: 2026-10-22

## Context and Problem Statement

Persephone must send and read end-to-end encrypted messages (P4). The pinned
Matrix SDK (0.19.1) shares room keys with **every** device in a room by default
(`CollectStrategy::AllDevices`) and decrypts messages from **any** device
(`TrustRequirement::Untrusted`). A homeserver, or anyone controlling it, can add
a device to a user's account. With the defaults, that device would receive the
keys for messages sent to the user, and its messages would be shown as the
user's. The project is engineered to withstand a targeted attack, so which
devices may read what Persephone sends, and whose messages it shows, must be
decided explicitly. Unencrypted rooms also need a sending rule, and the
secrets these flows need must not reach the web view (ADR-0004).

## Decision Drivers

* A device added by a homeserver must not read new messages or impersonate a
  contact.
* Follow the Matrix guidance for excluding insecure devices (MSC4153), which
  the SDK documents as its recommended strategy.
* Never send in plain text after an encrypted send fails.
* Keys, passwords and recovery keys stay in the core (ADR-0004); the SDK owns
  the cryptography (ADR-0002).
* Keep the first release usable with other clients that follow MSC4153.

## Considered Options

* Recipients: identity-based (only devices signed by their owner's
  cross-signing identity); all devices, failing on problems with verified
  users; only devices the user verified.
* Incoming: cross-signed or legacy sessions; cross-signed only; everyone.
* Unencrypted rooms: read only; send after a confirmation; send freely.

## Decision Outcome

Chosen options (owner decisions, 22 October 2026):

* **Recipients: `CollectStrategy::IdentityBasedStrategy`.** Room keys go only
  to devices signed by their owner's current identity. A user without a
  cross-signing identity receives none. The SDK refuses to send at all until
  this account has an identity that this device has verified, so setting up
  or verifying the account's identity is part of the first slice.
* **Incoming: `TrustRequirement::CrossSignedOrLegacy`.** Messages from devices
  not signed by their owner are shown as a notice, not as text. Sessions from
  before trust was recorded are still shown.
* **Unencrypted rooms: sending is allowed after a confirmation once per
  room.** The room is labelled as not encrypted. A send to an encrypted room
  never falls back to plain text.
* **Secrets in the core's own windows.** The account password needed to set
  up cross-signing and the recovery key are entered in the core's native
  prompt. A new recovery key is shown in a native window and is not stored by
  Persephone. Verification emoji, which are public comparison data, are shown
  in the page.
* Cross-signing and key backups are never created automatically; the user
  starts them.

### Positive Consequences

* A device injected through the homeserver gets no keys for what Persephone
  sends, and its messages are not shown as a contact's.
* Behaviour matches other clients' "exclude insecure devices" mode.
* No secret needed for encryption setup crosses the UI boundary.

### Negative Consequences

* People without cross-signing cannot read Persephone's messages, and their
  messages appear as notices. Some older clients never set it up.
* A new Persephone device cannot send until it is verified, from another
  device or with the recovery key.
* An unverified contact who changes identity still receives keys for their new
  identity's devices (the SDK raises no error); warning about this needs its
  own UI, which is not in the first slice.
* Lost recovery keys cannot be shown again; the key must be reset.

## Links

* [ADR-0002](0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md)
* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [Development plan: send text and reconcile history](../architecture/development-plan.md#send-text-and-reconcile-history)
* [MSC4153: exclude insecure devices](https://github.com/matrix-org/matrix-spec-proposals/pull/4153)
