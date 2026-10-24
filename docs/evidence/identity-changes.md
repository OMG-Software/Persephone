# Identity changes: sending pauses until the user decides

Checked 23 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. Second part of the remaining P4 work (PR E),
built on [contact verification](contact-verification.md) and implementing
[ADR-0007](../adr/0007-block-sending-to-a-changed-contact-identity-until-approved.md).
**Result: on both servers, after a contact reset their cross-signing
identity, Persephone sent nothing in the direct-message room, and the contact
received nothing, until the user accepted the new identity in the core's own
window or verified it by comparing emoji. A "no" there changed nothing. After a
contact Persephone had verified reset their identity, sending stayed refused
until the verification was withdrawn; the new identity could then be verified
again.**

## Design

* **Detection** ([`identities.rs`](../../crates/core/src/identities.rs)): the
  SDK pins the first identity it sees for each user. It reports a room's
  members whose identity changed only as a stream, whose first item is the
  current state when any member is in "pin violation" (changed, never
  verified) or "verification violation" (changed after this account verified
  it). The core polls it once without waiting; no item means no change.
* **Refusal** ([`sending.rs`](../../crates/core/src/sending.rs)): before each
  send in an encrypted room, any changed member refuses it as
  `identity_changed`, and a failure to tell refuses it as `failed`. Nothing
  reaches the homeserver. The pending message keeps its transaction ID and can
  be retried after the change is resolved.
* **Acceptance** (`accept_identity`): the page names a room and a user. The
  core checks that the user's identity changed in that room, then asks in its
  own window, with "No" as the default. Only a yes queues the change. The
  security thread checks again that the change is still there and of the
  same kind, then pins the new identity, or withdraws the verification of the
  old one, which also pins. An answer given after a lock is dropped.
* **Notification**: the sync loop takes the SDK's identity-update stream each
  round and announces a security change, so the page fetches the room's
  changes and the contact's trust again. The DM header reads "Identity
  changed" or "Identity changed since you verified them".
* **Page**: a banner in the room says sending is paused, names each changed
  member (at most 16, and says when more are not listed), and offers "Accept
  new identity…". For an unverified direct-message contact it also offers
  "Verify instead". For a contact who was verified, it says that accepting
  withdraws the verification, which can then be done again.
* **After a contact match**: the core asks the homeserver once more for the
  contact's identity (see the findings).

## Tests

New or changed, all passing (see the [ledger](verification.json)):

* Core: the page's list of changed members is bounded at 16, and a member
  left out, or whose ID could not be named back, marks the list truncated;
  the development answer file accepts only `yes`, is read each time, and is
  refused outside `.local`.
* Account (mock homeserver): a direct-message room with no changed identity
  lists none; accepting is `not_found` and asks nothing; an unknown room is
  `not_found`; malformed room IDs, non-user IDs and oversized user IDs are
  refused. The three contact tests now lock their session at the end (see the
  findings).
* Protocol: `identity_changes` and `accept_identity` carry only room and user
  IDs; missing IDs, extra fields (`all`, `confirmed`) and a `pin_identity`
  call are refused.
* UI host: only the IDs cross; oversized ones are refused before the core; a
  reply outside the contract is refused.
* Frontend: the banner names members as isolated, escaped text, offers "Verify
  instead" only for an unverified direct-message contact on a verified device,
  explains the withdrawal for a verified one, says when the list is cut, and
  disables its buttons while the core's window asks; the header shows both
  changed trust levels and never a shield for them.

## Live runs

The test peer ([`p4_peer`](../../tests/matrix/src/bin/p4_peer.rs)) acted as Bob
in an encrypted direct-message room with Persephone, set up as for
[contact verification](contact-verification.md). Bob replaced his identity
with `reset-identity`, which uses the SDK's cross-signing reset; after his own
reset he also withdrew his old verification of Alice, because his client
otherwise refuses to send to her (see the findings). The core's window was
answered from a development file (`PERSEPHONE_DEV_IDENTITY_ANSWER_FILE`),
`no` and then `yes`. The driver is `.local/runs/p4d-driver/drive_e.py`.

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| S1: send before any change | Bob decrypts it | same |
| S2: Bob resets; page | "Identity changed", room paused, Bob listed | same |
| S3: send | `identity_changed`; Bob receives nothing in 15 s | same |
| S4: accept, window says no | `cancelled`; still paused | same |
| S5: accept, window says yes; retry | pinned (`unverified`); pause gone; Bob decrypts the retry | same |
| S6: Bob resets again; "Verify instead" | same 7 emoji, done; `verified`; pause gone; next message decrypted | same |
| S7: Bob resets after being verified | `verified_changed`; no "Verify instead"; send refused; Bob receives nothing | same |
| S8: accept (withdraw); retry; verify from the header | `unverified`; retry decrypted; same 7 emoji, done; `verified` | same |
| Driver result | pass (`.local/runs/p4e-07`) | pass (`.local/runs/p4e-06`) |

### Findings

* **A verified contact's change blocks the verification request too.** "Verify
  again" first failed on both servers: a contact verification request is an
  encrypted message in the direct-message room, and the SDK refuses every
  encrypted send there while a verified member is in verification violation.
  The order is therefore withdraw, then verify (ADR-0007 records this as a
  constraint, not a separate decision). A pin violation does not stop the
  SDK's own sends, so an unverified contact can be verified instead.
* **The same happens to the person who reset.** After Bob's reset, his new
  identity has not signed Alice, so his client sees the Alice he had verified
  as changed and refuses to send her the verification reply. A real user would
  need to withdraw that verification as well; the peer does so.
* **Merovingian: a contact verified after a change stayed "changed".** In two
  runs, both sides finished the comparison, but Persephone read Bob as
  changed (not verified) for over a minute, and as verified after a restart.
  The core now asks the homeserver for the contact's identity once a contact
  verification is done; the next Merovingian run passed. Most likely nothing
  had started an earlier keys query; this is not shown.
* **wiremock reuses mock servers.** The new account tests ended without
  locking their session, so their sync loops kept polling a mock server that
  the pool then gave to another test, which failed its sync-rate assertion
  intermittently. The tests now lock their session at the end; the two PR D
  tests got the same fix in that pull request.
* `bootstrap` on a device that already holds its cross-signing keys uploads
  the same identity again; a reset needs the SDK's `reset_cross_signing`.

### Attempts not counted

Under `.local/runs/p4e-01` to `p4e-06`:

* `p4e-01`: the peer's `bootstrap` did not change Bob's identity (above).
* `p4e-02`: both servers passed S1 to S7; "Verify again" failed on Synapse
  (the first finding), and Merovingian lost its DevTools connection at that
  step.
* `p4e-03`: Synapse passed through the withdrawal, then Bob could not reply
  to the new request (the second finding); Merovingian stopped after S6 on a
  page call whose error the driver did not record.
* `p4e-04` and `p4e-05`: Synapse passed every step in `p4e-05` (the driver
  read trust once too early in `p4e-04`); Merovingian showed the stale
  verified state (third finding).
* `p4e-06`: Synapse passed every step with the final build; the Merovingian
  process exited without a logged error after registering the accounts, so
  three sign-in attempts failed; the next harness start (`p4e-07`) passed.
  MSI Afterburner and RTSS were running throughout, as in earlier runs.

## Limits

* The check runs just before each send; an identity that changes between the
  check and the SDK's key sharing still receives that one message's key.
* Messages from a contact's new identity are still shown (ADR-0007).
* Only direct-message rooms were run live; a group room pauses the same way,
  but no group run was made, and accepting in a group asks once per member.
* The native confirmation window has not been checked by hand: the runs used
  the development answer file.
* A homeserver that hides an identity change from Persephone is not detected.
* Debug builds on one Windows host only.
