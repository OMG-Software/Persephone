# Save the recovery key to a file from the core

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-27

Technical Story: on 27 October 2026 the owner asked to download the recovery
key and to change it, and decided that the core saves the key with a native
Save dialog.

## Context and Problem Statement

The recovery key opens the account's secret storage (ADR-0005). The core
shows a new key once in its own window and does not keep it, so it cannot
show or download an existing key later. Users want a file copy, and a way to
replace a key they think was exposed. ADR-0004 forbids the key in the web
view, and the page must not name paths. How can the key be saved and
replaced?

## Decision Drivers

* The key never crosses to the UI process (ADR-0004), and the page names no
  path (as for the image picker, ADR-0009).
* Do not keep the key anywhere Persephone controls after it is shown.
* Replacing the key must be deliberate: the old key stops working.
* Use the SDK's recovery; no custom cryptography.

## Considered Options

* Keep the key encrypted in the profile and offer it for download later.
* Save the key from the page through a file download.
* Offer a Save dialog in the core whenever a new key is shown, and let the
  user make a new key to get a file copy.

## Decision Outcome

Proposed option: "offer a Save dialog in the core whenever a new key is
shown", because the key then exists only for as long as it is shown and
saved, and never in the page.

* After the core's window shows a new key (set-up, recovery set-up, or a
  change), the core opens the platform's own Save dialog
  (`GetSaveFileNameW` on Windows, `NSSavePanel` on macOS), which asks before
  replacing a file. Cancel skips saving.
* The core writes the file itself: the key and a line break only. A link,
  junction, directory or special file at the chosen path is refused. The
  key goes to a new temporary file beside it, is flushed to disk, and only
  then is renamed over the chosen path, so a failed write leaves an existing
  file as it was. On Unix the file is readable by its owner only; on
  Windows it takes its folder's rules. A failed write is
  `recovery_key_not_saved`; the key was shown.
* `change_recovery_key` takes no argument. Offered only to a verified device
  whose recovery is enabled, it asks in the core's own window, with "No" as
  the default, whether to make a new key and warns that the old one stops
  working. On yes it calls the SDK's `recovery().reset_key()`, then shows and
  offers to save the new key as above.
* "Download" therefore means "make a new key and save it": the page says so.

### Positive Consequences

* Users get a file copy without the key ever reaching the web view.
* A key that may be exposed can be replaced in one deliberate step.

### Negative Consequences

* The file is outside Persephone's protection once written; the user must
  keep it safe. File-system copies and backups are outside any erasure
  guarantee.
* An existing key cannot be downloaded again; only a new one can.
* Linux has no native window yet, so the key cannot be shown or saved there.
* The change was tested against a stateful mock homeserver only, not yet
  against Merovingian or Synapse.

## Links

* Keeps [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Extends [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
