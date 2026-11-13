# Use an existing key backup, or start a new one after a confirmation

* Status: accepted
* Deciders: project owner (decision in the development session, 13 November 2026)
* Date: 2026-11-13

Technical Story: amends [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
("cross-signing and key backups are never created automatically") and builds on
[ADR-0021](0021-save-the-recovery-key-to-a-file-from-the-core.md)

## Context and Problem Statement

An account set up by another client often already has a key backup on the
homeserver. Before this record, Persephone could not use it or replace it:

* Setting up recovery on a verified device failed with
  `backup_exists_on_server`, because matrix-sdk 0.19.1 `Recovery::enable`
  refuses to create a backup while one exists.
* The recovery key was accepted only on an unverified device. A verified
  device whose recovery was `incomplete` (secret storage without a usable
  backup key) had no way to take the backup key from secret storage.

The account then had no working backup, and the page offered a step that was
certain to fail. How should Persephone use such a backup, and what should it
do when the backup's key is lost?

## Decision Drivers

* Deleting a key backup destroys message keys that may exist nowhere else. It
  must never happen automatically or from the page alone (ADR-0005).
* Secrets stay in the core: the recovery key is entered and shown only in the
  core's own windows (ADR-0005, ADR-0021).
* Use SDK-owned recovery and backup operations; no custom cryptography.
* Fail closed: offer nothing on a guess about the homeserver's state.

## Considered Options

* Use the backup through the recovery key, and offer a new backup only after a
  confirmation in the core's own window.
* Use the backup through the recovery key only; never delete a backup.
* Replace an unusable backup automatically when recovery is set up.

## Decision Outcome

Chosen option: "Use the backup through the recovery key, and offer a new
backup only after a confirmation in the core's own window", because it lets a
user with the key keep every message, and a user without it recover a working
backup, while the destructive step stays explicit.

* **Use the existing backup.** `recover_with_key` is also accepted on a
  verified device whose recovery is `incomplete`. The SDK opens secret storage
  and takes the backup key when it matches the homeserver's backup. A key that
  opens secret storage, but finds no matching backup key there, reports
  `backup_key_unavailable`.
* **Offer a new backup only when the backup is unusable.** The core sets
  `can_start_new_backup` when this device is verified, holds all private
  cross-signing keys, has no backup enabled, and either recovery is
  `incomplete` or recovery is `disabled` while the homeserver keeps a backup.
  A failure to read the homeserver's state offers nothing. "Set up recovery"
  is not offered at the same time, because it would fail on the same backup.
* **Confirm in the core.** `start_new_backup` takes no argument. The core asks
  in its own window, with "No" as the default, and states that messages kept
  only in the old backup become unreadable. Only after "Yes" does it ask the
  homeserver again, delete every backup version, and enable recovery. The new
  recovery key is shown and saved as in ADR-0021; it never crosses the page
  boundary.
* A platform without the confirmation window fails closed with `unsupported`.

### Positive Consequences

* An account with a backup from another client can use it on this device.
* A lost backup key no longer leaves the account without a working backup.
* The page cannot delete a backup by itself: the decision is made in the
  core's window after a fresh check.

### Negative Consequences

* A "Yes" is final. Keys held only in the old backup cannot be read again.
* If the deletion succeeds and the new set-up fails, the account has no backup
  until the user sets up recovery again; the status then offers it.
* The core trusts the SDK's cached answer about the homeserver's backup for the
  offer. It asks again before it deletes, so a stale cache can only show or
  hide the offer, not delete a backup that no longer exists or is now usable.
* Each supported platform needs its own confirmation window. Until one exists,
  the step is unsupported there.

## Pros and Cons of the Options

### Recovery key, plus a confirmed new backup

* Good, because the user keeps every message when they have the key.
* Good, because the destructive step is explicit and confirmed outside the page.
* Bad, because it adds a destructive command to the core's protocol.

### Recovery key only

* Good, because nothing is ever deleted.
* Bad, because a user who lost the key can never have a working backup again
  in Persephone.

### Replace automatically

* Good, because it needs no extra step.
* Bad, because it silently destroys keys and contradicts ADR-0005.
