# Sending: encrypted text, consent for unencrypted rooms, delivery states

Checked 22 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. Second part of the P4 slice (PR B of three),
built on the [encryption setup](encryption-setup.md). **Result: on both
servers, Persephone sent an encrypted message that a cross-signed recipient
decrypted, read the recipient's reply, refused to show a message from the
recipient's unsigned device (which in turn could not read Persephone's next
message), and sent in an unencrypted room only after the user confirmed.**

## Design

* **Outbox** ([`outbox.rs`](../../crates/core/src/outbox.rs)): the page sends
  only a room ID and the text. The core assigns a transaction ID and a local
  ID, queues the message (at most 16 unsent; sent ones kept up to 32 so the
  page can match them to timeline events) and replies at once. Memory only:
  unsent messages are dropped on lock or restart.
* **Order**: one `core-send` thread (one lease per send, joined by lock) takes
  messages in order. A later message in a room waits while an earlier one
  there is sending, failed or unknown; other rooms carry on.
* **Send** ([`sending.rs`](../../crates/core/src/sending.rs)): the room's
  encryption is read, from the homeserver if not known. A room that is not
  encrypted needs the user's recorded consent, or the message fails as
  `confirmation_required` and nothing is sent. In an encrypted room the SDK
  shares keys under the identity-based policy and encrypts, or fails before
  any request.
* **Outcomes**: `sent` with the event ID; `failed` with a code
  (`device_not_verified` for an unverified own identity, `identity_changed`,
  `not_joined`, `rejected` for a homeserver refusal, `failed`); `unknown` for
  a transport failure, when the request may have reached the homeserver. Retry
  sends the same transaction ID, so the homeserver recognises a repeat; the SDK
  is built without retries, so nothing is resent behind the user.
* **Consent**: a list of room IDs (at most 200) in an encrypted crypto-store
  custom value, so the user is asked once per room. The page asks with an
  `alertdialog` before the first send; the core enforces it either way.
* **Views and page**: room summaries say whether a room is encrypted; rooms
  that are not are labelled in the list and the timeline. A composer sends
  from the timeline and is disabled, with the reason, when this device is not
  verified and the room is encrypted. Messages being sent are listed with
  their state as fixed text, with Retry, Discard and (for a missing consent)
  "Send unencrypted"; a sent one disappears once the timeline shows it.

## Tests

New, all passing (the full check passed 293 tests; see the [ledger](verification.json)):

* Outbox: a failed message holds later ones in its room while other rooms
  carry on, and a retry keeps the transaction ID; retry and discard apply only
  where safe; the unsent and kept limits hold.
* Account (mock homeserver, development feature):
  * an unverified device sending in an encrypted room fails as
    `device_not_verified` with **no send request at all** (encrypted or
    plain); lock joins the sending thread and drops unsent messages;
  * an unencrypted room fails as `confirmation_required` with no request until
    consent; after consent a retry sends `m.room.message`, and the next message
    is sent without asking again;
  * a refused send keeps the next message in that room queued; the retry sends
    the same transaction ID path again, the next message its own;
  * bad input or no account queues nothing.
* Core unit: body and room checks (line breaks and tabs allowed; empty, blank,
  oversized or control-bearing bodies refused).
* Protocol: sending calls refuse extra fields such as `txn_id` or `encrypted`
  and out-of-range IDs. UI host: oversized bodies never reach the core; a
  reply carrying a transaction ID is refused.
* Frontend: unencrypted labels; every pending state as fixed text, escaped
  bodies, and only the safe actions per state.

## Live runs

A new test peer ([`p4_peer`](../../tests/matrix/src/bin/p4_peer.rs)) signed in
as the harness's second account ("Bob"), set up Bob's cross-signing, created
the rooms and checked what Bob received; a second, never cross-signed Bob
device used the SDK's default key sharing. Persephone ran as the first account
through the real page (development core).

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| Persephone sends in an encrypted room | Bob decrypts it; it arrived encrypted | same |
| Bob replies | shown in Persephone | same |
| Bob's unsigned device sends | Persephone shows the untrusted-device notice, never the text | same |
| Persephone sends again | Bob reads it; the unsigned device cannot (3 undecryptable events) | same |
| Unencrypted room | labelled; confirmation shown; nothing received before consent; plain text after | same |
| Recovery key in the page | no | no |
| Exit codes | 0 | 0 |

* Merovingian **encrypts new private rooms by default**: a private room
  created without encryption state still had `m.room.encryption` (checked on
  the server). Persephone correctly treated it as encrypted (no label, no
  confirmation, encrypted send). The unencrypted path on Merovingian used a
  public room; on Synapse a private one.
* Three Merovingian attempts lost the DevTools connection when WebView2's
  browser process crashed at start; the two dumps checked show the same
  `RTSSHooks64.dll` crash as in the [window retry evidence](window-retry.md).
  Those attempts are not counted. Later attempts reused the account and
  verified the new device with the recovery key instead of setting it up.

## Limits

* `unknown` (a transport failure) is covered by unit tests and code review,
  not by a mock or live run: the mock server cannot drop a connection, and the
  request timeout is 30 s.
* Pending messages are in memory only; a message unsent at a lock or restart is
  gone, and the page says nothing about it after a restart.
* Verifying other people, and approving a contact's changed identity, are not
  built; `identity_changed` only reports the failure.
* One of 28 repeated runs of the account tests failed, early, before its
  output was captured; the other 27 passed. The cause is not known.
  *Later correction (22 October 2026, during PR C):* the same test failed twice
  more with its output: the unverified-device test sent before the first
  sync, so the room was unknown (`not_joined`), or known with its encryption
  not yet settled, when the mock answered "no encryption" for every room
  (`confirmation_required`). Both are correct answers for that state, and
  nothing was sent. The test now waits for the room, and the mock answers with
  the room's real encryption state; 30 further runs passed, on the PR C branch.
  That the earlier failure had this cause is likely, not shown.
* Debug builds on one Windows host only.
