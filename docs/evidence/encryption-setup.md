# Encryption setup: cross-signing, recovery and the key policy

Checked 22 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. First part of the P4 slice (PR A of three):
the encryption policy of [ADR-0005](../adr/0005-share-room-keys-only-with-cross-signed-devices.md),
setting up cross-signing and recovery for a new account, and verifying a new
device with the recovery key. Sending (PR B) and verification from another
device (PR C) follow. **Result of the first live runs: on both servers, a
fresh account was set up from the page in about 3 seconds without the page
ever holding the password or the recovery key; a second device of the same
account showed as not verified, was verified with the recovery key, and stayed
verified after a restart. A wrong recovery key was refused on Merovingian; that
step was not run on Synapse.** These runs tested the first version. Later
review rounds changed the code, and their live coverage is listed under
[Live runs](#live-runs).

## Design

* **Policy** ([`account.rs`](../../crates/core/src/account.rs) `build_client`,
  for sign-in and restore): room keys go only to devices signed by their
  owner's identity (`IdentityBasedStrategy`); messages are decrypted only from
  cross-signed devices or legacy sessions (`CrossSignedOrLegacy`); the SDK never
  sets up cross-signing or backups on its own; after recovery, a key missing
  for a message is fetched from the backup when that message fails to decrypt.
* **Late keys** ([`account.rs`](../../crates/core/src/account.rs)
  `take_key_rooms`, [`session.rs`](../../crates/core/src/session.rs)
  `run_sync`): a room key can arrive after its message was shown as
  undecryptable, from another device or from the backup download that the
  failure started. After each sync round the core takes the rooms named by the
  SDK's room-key stream (at most `MAX_ROOMS`; more, or a lagged stream, counts
  as any room, which the core applies to the rooms the page has open without
  listing every joined room) and tells the page, which fetches an open room's
  newest page again and replaces each notice whose message now decrypts.
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
  (8 MiB stack, one lease per step, joined by lock) asks the homeserver
  whether an identity is published (a raw keys query, every 30 s while there
  is none), then reads the local store every 2 s. An identity only this device
  holds, left by a set-up whose upload failed, counts as none, so set-up can be
  retried; the retry uploads the same keys. Changes
  raise a content-free `security_changed` event; the page fetches
  `security_status` without clearing anything else.
* **Set up** (only when the homeserver says there is no identity, checked
  again just before): the core asks for the password in its own Windows prompt
  (username fixed to the signed-in account), answers the homeserver's
  authentication round, enables recovery (secret storage plus a new key
  backup), and shows the recovery key in a native window. Persephone does not
  keep the key.
* **Set up recovery** alone: when cross-signing was set up but recovery
  failed, the core offers recovery again (only if this device holds the
  private cross-signing keys of a published identity). It needs no password
  and shows the new key in the native window.
* **Recover**: the core asks for the recovery key in its own prompt; the SDK
  imports the cross-signing keys from secret storage, marks the identity
  verified and signs this device.
* **Prompts never hold a lease**: they run on short-lived threads, so a prompt
  left open does not delay a lock; an answer given after a lock is dropped.
* **No stale writes**: every change to the encryption state (the thread's
  results, a queued command, a prompt's answer, a failed key window) is checked
  against the session's generation under the lock a lock takes to advance it,
  so nothing from before a lock reaches the locked or the next session. The
  status also says whether this device can set up recovery alone, and the page
  offers it only then.
* **Contracts and calls**: `security_status`, `set_up_encryption`,
  `recover_with_key` take no arguments and reply with codes only; a reply with
  any extra field (such as a key) is refused by the UI host.

## Tests

New, all passing. The test counts of each review round are in the
[ledger](verification.json).

* Account (mock homeserver, development feature):
  * set-up asks for the password once, answers a 401 authentication round with
    the round's session, shows a recovery key, and the key appears in no reply
    and no profile file; set-up is not offered again once an identity exists;
    lock joins the security thread and forgets the state;
  * a cancelled prompt uploads nothing with a password and reports
    `cancelled`; a wrong password reports `invalid_password`; both leave the
    account at `none`, and set-up with the right password then succeeds;
  * a key backup that fails once leaves the identity without recovery;
    setting up recovery alone then succeeds without a second password, and
    set-up as a whole is not offered over the identity;
  * an answer given after a lock is never sent;
  * a lock during set-up still shows the new recovery key
    (`given_a_lock_during_set_up_then_the_new_recovery_key_is_still_shown`;
    it fails on the code before the third round);
  * a recovery-key window that fails at once leaves "key not shown" in the
    status (`given_a_failing_key_window_then_set_up_reports_the_key_was_not_shown`).
    This test also passed on the order before the fourth round, so it cannot
    detect that race.
* Session unit: a write meant for the session before a lock is refused after
  the lock and after a later unlock, and leaves no status or command.
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

After the review fixes (a published-identity check by a raw keys query, retry
after a refused password, recovery alone), the probe was repeated on Synapse
and passed the same steps on the first attempt. The Merovingian repeat was
blocked by this host: three attempts lost DevTools as the WebView2 browser
process crashed (four dumps in nine minutes, the `RTSSHooks64.dll` crash of
the [window retry evidence](window-retry.md)); the account tests cover the
fixes.

Three review rounds followed these live runs. None of them has a live run on
this branch alone. The second round ran live later, on the PR C branch; the
third and fourth have no live run:

* The second round sends every encryption-state write through a check of the
  session generation. That covers the thread's results, queued commands,
  prompt answers and a failed recovery-key window. A write that belongs to a
  session that was locked meanwhile is dropped. The round also adds
  `canSetUpRecovery`, and it maps an unreadable development recovery file to
  `failed`. This changes the normal path. The same code later ran live as
  step A (set up encryption and recovery) of the own-device verification runs
  on both servers, on the PR C branch
  ([#11](https://github.com/OMG-Software/Persephone/pull/11)), which also holds
  sending and verification.
* The third round shows a new recovery key even when a lock or sign-out
  starts during set-up. Before, the key was dropped, and recovery stayed
  enabled with a key nobody saw. An account test covers this case; no live run
  does.
* The fourth round writes set-up's result before it starts the key window.
  Without that order, a window that failed at once could have its
  "key not shown" error replaced by the result. A new account test checks
  that the error stays. It passed 20 of 20 runs on the old order too, because
  that race is hard to hit, so the order, not the test, is the guarantee. No
  live run.

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
* The mock homeserver does not sync account data back, so the account tests
  cannot see recovery become enabled; the live runs do.
* Late keys reach the page at the end of the next successful sync round,
  normally within `SYNC_POLL` (5 s) of the key being stored. This is not an
  upper bound: keys are not taken after a failed round, so sync failures
  delay them until a round succeeds, and one round takes at most 64 batches,
  leaving any later ones for the following rounds. Only an open room's newest page
  is fetched again; an older page already loaded keeps its notices until the
  room is opened again. The unit test imports keys the way a backup download
  stores them; no live run has checked keys arriving from another device or
  the backup, and the sync loop's handover to the page is not tested end to
  end.
* Not tested: an account whose identity was reset elsewhere, a homeserver that
  refuses key backup, other users' devices (no messages yet), and Linux and
  macOS (no prompt there: set-up and recovery report `unsupported`).
