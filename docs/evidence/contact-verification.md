# Contact verification: comparing emoji with another person

Checked 23 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. First part of the remaining P4 work (PR D),
built on [own-device verification](own-device-verification.md). **Result: on
both servers, Persephone verified a contact in an existing direct-message
room, both when Persephone asked and when the contact asked, after the user
saw the same seven emoji on both sides; the contact's client then also saw
Persephone's account as verified. When the user said the emoji differ, the
flow was cancelled on both sides and the contact stayed unverified.**

## Design

* **Direct-message rooms only** ([`contacts.rs`](../../crates/core/src/contacts.rs)):
  a room is a direct-message room when the account's `m.direct` data marks it
  for exactly one other user and it has exactly two members, this account
  and that user. The marks can outlive membership changes, so a room a third
  person joined has no contact. The room list names that contact (`contact`),
  and a direct-message room without a name of its own is named after them.
* **No room created, nobody invited**: the SDK sends a contact verification
  request to the first direct-message room it finds with that user
  (`get_dm_room`); with none it creates one, and if the contact left it, it
  invites them back. The core refuses (`not_direct_room`) unless both the
  room the page names and the room the SDK will use have exactly two members,
  this account and the contact, both joined. A membership change between
  that check and the SDK's own is still possible: the SDK 0.19.1 offers no
  public way to send the request without its invitation fallback.
* **Only a verified device vouches for someone**: the page offers "Verify"
  only on a verified device, and the core queues the start only then. A match
  signs the contact's identity with the account's user-signing key, so both
  starting and accepting need this device to hold that key
  (`cannot_sign_contacts` otherwise); without it the SDK would end the flow as
  done without verifying anyone.
* **Incoming requests**: a request in a room is taken only when it is
  addressed to this account and comes from the contact of the direct-message
  room it arrived in. One flow at a time, as before; the flow's view now
  carries the contact's user ID (untrusted, projected like a name).
* **Trust** (`contact_status`): `verified`, `unverified`, `no_identity` or
  `unknown`, read from the local crypto store. Starting a verification also
  asks the homeserver before it reports `contact_has_no_identity`. Sync loads members lazily, so
  the SDK may not follow a contact's keys yet: when the store has nothing, the
  core loads the room's members (from then on the SDK follows every member's
  keys) and asks the homeserver once for the contact's keys. A failure reads
  `unknown`, never `no_identity`.
* **Page**: the room header shows the contact's user ID and trust as fixed
  text, a shield when verified, and "Verify" when this device may vouch for an
  unverified contact. The verification screen names the contact and asks the
  user to compare the emoji with them in person or on a call they trust, not
  in the chat. A step the core could not carry out is shown there, and the
  wait after confirming can be cancelled (see the findings).

## Tests

New or changed, all passing (see the [ledger](verification.json)):

* Core, session: only a verified device queues a contact verification, and not
  while another flow is open; a confirmed flow can still be cancelled.
* Core, hub: a change to the account's direct-message rooms refreshes only the
  room list.
* Account (mock homeserver, development feature): only the direct-message room
  names its contact, and is named after them; a room still marked direct with
  the contact that a third person joined names no contact and is `not_found`; a contact without an identity
  reads `no_identity`; any other room is `not_found`; empty, malformed and
  oversized room IDs are refused; on a device that is not verified, asking to
  verify a contact queues nothing and sends nothing to the room.
* Protocol: `contact_status` and `verify_contact` carry only a room ID; extra
  fields (`user_id`, `create_room`), missing or null room IDs, and a
  `verify_user` call are refused.
* UI host: only the room ID crosses; an oversized one is refused before the
  core; a trust level outside the contract is refused; the user ID comes back
  unchanged for the page to render as text.
* Review changes (23 October 2026, no live run): the two-member rule above,
  the homeserver lookup before `contact_has_no_identity`, contact replies
  applied only when newer than the one shown, and a verification failure shown
  under the header only for the room whose contact the last action verified.
* Frontend: the header shows the contact and trust as isolated, escaped text,
  offers "Verify" only to a verified device and only for an unverified
  contact, never reads an unknown trust level as verified, and other rooms keep
  their room icon; the verification screen names the contact in every phase,
  shows no device steps, and offers Cancel after confirming.

## Live runs

The test peer ([`p4_peer`](../../tests/matrix/src/bin/p4_peer.rs)) signed in
as the second account ("Bob"), set up cross-signing and created an encrypted
direct-message room with the first account. A second device of the first
account (the peer's "Alice") joined it and marked it as direct, because
Persephone has no invitation UI yet. Persephone ran through the real page
(development core), driven over WebView2's DevTools port; the driver is in
`.local/runs/p4d-driver`. Persephone set up encryption first, so it held the
user-signing key.

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| Room list names Bob as the contact; header says "Not verified" | yes | yes |
| F: Bob asks, Persephone accepts, says the emoji differ | same 7 emoji; cancelled on both sides; Bob still not verified | same |
| E: Persephone asks from the header, both confirm | same 7 emoji; done on both sides; header verified; Bob's client sees Alice verified | same |
| G: Bob asks again, both confirm | same 7 emoji; done | same 7 emoji; Persephone's confirmation refused (429); Cancel from the page; repeated after 60 s: done |
| Driver result | pass | pass |

Raw reports are in `.local/runs/p4d-07` (one harness start, both servers).

### Findings

* **Sliding Sync loads members lazily, so the SDK did not follow the
  contact's keys.** The first run never learned Bob's identity: Persephone's
  Sliding Sync asks only for its own membership, and the SDK follows a user's
  keys only once it sees their membership in an encrypted room. Sending works
  because a send loads the full member list first. The core now loads the
  direct-message room's members, and asks for the contact's keys, when the
  store has no identity for them.
* **`m.direct` changes did not refresh the room list.** Account data touches
  no room, so a sync round that brought only the new `m.direct` reported no
  change, and the page kept a room list without the contact. The sync loop now
  announces a room-list change when `m.direct` changes.
* **Synapse rate-limits room messages, and the core does not resend.** The
  third flow in a row on Synapse sent Persephone's confirmation into Synapse's
  message rate limit (two `429` replies to Alice's encrypted room sends in
  the server log; none on Merovingian). The core's SDK client is built without
  automatic retries ([sending](encrypted-sending.md)), so the confirmation was
  never sent, the flow waited in `confirmed`, and the page had no way out. The
  page now shows "That did not work." and offers Cancel in that phase; a new
  flow a minute later finished.
* An unnamed direct-message room was named "2 people" by the SDK, because
  Sliding Sync sends no member names; it is now named after its contact.

### Attempts not counted

Earlier harness starts under `.local/runs/p4d-01` to `p4d-06`:

* Two runs lost the DevTools page connection right after sign-in (Merovingian
  in `p4d-01`, Synapse in `p4d-04`), and the core then waited for the user to
  answer its window-retry offer. MSI Afterburner and RTSS were running, as in
  the [window retry evidence](window-retry.md); no crash dump was checked, so
  neither is shown to be the cause.
* The first run found the lazy-member gap above (`p4d-01`); one run
  (`p4d-04`, Merovingian) found the stale room list. Both are fixed above.
* One run reused accounts that already had an identity, so set-up was not
  offered (`p4d-01`, Synapse); the driver needs fresh accounts.
* Driver defects: one harness start lost both reports to a text-encoding
  error (`p4d-02`), and one compared the emoji wrongly (`p4d-03`; the emoji
  were in fact the same).
* `p4d-05` and `p4d-06` passed on Merovingian and failed flow G on Synapse as
  described above, before the page offered Cancel after confirming.

## Limits

* Verifying from the member list of other rooms, and creating a
  direct-message room to verify in, are not built. Persephone cannot accept
  invitations yet, so the live runs joined the room from another device.
* A contact whose identity changes is not handled yet (P4, PR E):
  `identity_changed` only reports the failure, and an unverified contact's
  new identity still receives keys (ADR-0005).
* A rate-limited verification step is not sent again; the user cancels and
  starts again. A request refused by the homeserver before the flow starts
  reads `failed`.
* Trust shown in the header is read when the room opens and after each
  encryption change; a contact's identity change from elsewhere shows on the
  next of those.
* The other side was an SDK peer, not another Matrix client; interoperability
  with Element or other clients is not checked. QR-code verification is not
  offered.
* Debug builds on one Windows host only.
