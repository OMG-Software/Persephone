# Encryption setup: cross-signing, recovery and the key policy

Checked 22 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. First part of the P4 slice (PR A of three):
the encryption policy of [ADR-0005](../adr/0005-share-room-keys-only-with-cross-signed-devices.md),
setting up cross-signing and recovery for a new account, and verifying a new
device with the recovery key. Sending (PR B) and verification from another
device (PR C) follow. **Result: on both servers, a fresh account was set up
from the page in about 3 seconds without the page ever holding the password or
the recovery key; a second device of the same account showed as not verified,
was verified with the recovery key, and stayed verified after a restart; a
wrong recovery key was refused.**

## Design

* **Policy** ([`account.rs`](../../crates/core/src/account.rs) `build_client`,
  for sign-in and restore): room keys go only to devices signed by their
  owner's identity (`IdentityBasedStrategy`); messages are decrypted only from
  cross-signed devices or legacy sessions (`CrossSignedOrLegacy`); the SDK never
  sets up cross-signing or backups on its own; after recovery, a key missing
  for a message is fetched from the backup when that message fails to decrypt.
* **Notices** ([`matrix_views.rs`](../../crates/core/src/matrix_views.rs)): an
  undecryptable event says why: `withheld` (the sender withheld the key),
  `untrusted_sender` (the sending device is not signed by its owner) or
  `undecryptable`. An encrypted event the SDK did not try to decrypt is never
  shown as anything else.
* **Security state** ([`security.rs`](../../crates/core/src/security.rs),
  [`session.rs`](../../crates/core/src/session.rs)): `identity` is `unknown`,
  `none` (no identity on the homeserver), `unverified` (an identity this
  device is not verified with) or `verified` (identity verified and this
  device signed by it); `recovery` follows the SDK. A `core-security` thread
  (8 MiB stack, one lease per step, joined by lock) asks the homeserver once
  whether an identity exists, then reads the local store every 2 s. Changes
  raise a content-free `security_changed` event; the page fetches
  `security_status` without clearing anything else.
* **Set up** (only when the homeserver says there is no identity, checked
  again just before): the core asks for the password in its own Windows prompt
  (username fixed to the signed-in account), answers the homeserver's
  authentication round, enables recovery (secret storage plus a new key
  backup), and shows the recovery key in a native window. Persephone does not
  keep the key.
* **Recover**: the core asks for the recovery key in its own prompt; the SDK
  imports the cross-signing keys from secret storage, marks the identity
  verified and signs this device.
* **Prompts never hold a lease**: they run on short-lived threads, so a prompt
  left open does not delay a lock; an answer given after a lock is dropped.
* **Contracts and calls**: `security_status`, `set_up_encryption`,
  `recover_with_key` take no arguments and reply with codes only; a reply with
  any extra field (such as a key) is refused by the UI host.

## Tests

New, all passing (the full check passed 276 tests; see the [ledger](verification.json)):

* Account (mock homeserver, development feature):
  * set-up asks for the password once, answers a 401 authentication round with
    the round's session, shows a recovery key, and the key appears in no reply
    and no profile file; set-up is not offered again once an identity exists;
    lock joins the security thread and forgets the state;
  * a cancelled prompt uploads nothing with a password and reports
    `cancelled`; a wrong password reports `invalid_password`;
  * an answer given after a lock is never sent.
* Core unit: undecryptable reasons map to their notices; secret checks;
  unsupported platforms fail closed for secrets and the key window; the
  development key file must be inside `.local`.
* Protocol: the new calls refuse extra fields (`password`, `recovery_key`); the
  security event refuses content.
* UI host: the commands relay codes only and refuse a reply carrying a key;
  the security event reaches the session channel as `security`.
* Frontend: the panel offers set-up, recovery key or nothing according to the
  state, never renders an input, and shows fixed text for each error; the new
  notices render as fixed text.

## Live runs

The development core answered the password from the harness profile and
wrote and read the recovery key through a file under `.local` (development
feature only). A DevTools probe drove the real page.

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| Device A, fresh account | "Encryption is not set up" | same |
| Set up encryption | "This device is verified. Recovery is set up." after 2.8 s | same, after 2.5 s |
| Recovery key in the page | no | no |
| Device B (new profile), same account | "not verified", recovery incomplete | same |
| Use recovery key | verified after 1.0 s | verified after 0.3 s |
| Device B restarted | still verified | same |
| Device C with a wrong recovery key | "That recovery key did not work", still not verified | not run |
| Exit codes | 0 for every run | 0 for every run |

The first Merovingian attempt lost the DevTools connection on device A, after
set-up had in fact completed (device B found the identity and recovered); that
run is not counted, and Merovingian was repeated on a fresh harness.

One change followed the live runs: a recovery-key window that fails after the
session was locked no longer reports its failure into the next session. It
touches only that failure path, which the runs did not reach.

## Limits

* Nothing can be sent yet (PR B), and verification from another device is not
  built (PR C), so an account set up elsewhere without recovery cannot verify
  this device yet.
* The native password prompt and recovery-key window have not yet been
  checked by hand; the runs used the development source.
* The SDK copies the password into its upload request and the recovery key
  into its import; those copies are outside the zeroing guarantee.
* If the recovery-key window cannot be shown, recovery stays enabled with a key
  nobody saw; the page says so, but resetting the key is not built.
* Not tested: an account whose identity was reset elsewhere, a homeserver that
  refuses key backup, other users' devices (no messages yet), and Linux and
  macOS (no prompt there: set-up and recovery report `unsupported`).
