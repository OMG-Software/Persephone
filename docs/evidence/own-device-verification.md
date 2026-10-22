# Own-device verification: comparing emoji with another device

Checked 22 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. Third part of the P4 slice (PR C of three),
built on the [encryption setup](encryption-setup.md) and
[sending](encrypted-sending.md). **Result: on both servers, a new Persephone
device was verified by another device of the same account, in both
directions (Persephone asked, or the other device asked), after the user saw
the same seven emoji on both sides; when the user said the emoji differ, the
flow was cancelled on both sides and the device stayed unverified.**

## Design

* **One flow** ([`verification.rs`](../../crates/core/src/verification.rs)):
  the security thread owns at most one SAS flow and advances it between its
  other steps, one lease per step. The flow is dropped inside the runtime when
  the thread ends (lock or sign-out).
* **Own devices only**: "Verify with another device" asks the account's own
  identity (`request_verification`), which the SDK sends only to own devices
  that are cross-signed. An incoming request is taken only from the account's
  own user, with a flow ID of at most `MAX_FLOW_ID_BYTES` (128); requests from
  other people are ignored in this slice.
* **Decisions stay with the user**: the core starts the comparison once the
  other device is ready, and accepts a comparison the other device starts.
  Accepting an incoming request, confirming that the emoji match, and saying
  they differ are the user's.
* **Page**: it sees a phase (`incoming`, `waiting`, `compare`, `confirmed`,
  `done`, `cancelled`), the seven emoji (public comparison data) and the other
  device's name (untrusted text, projected like any name and shown isolated).
  It acts by flow ID. The core refuses an action for another flow or phase,
  and every state write is checked against the session generation, so a lock
  discards it.

## Tests

New or changed, all passing (the full check passed 298 tests; see the
[ledger](verification.json)):

* Account (mock homeserver, development feature): with no identity and no
  flow, starting, accepting, confirming and cancelling change nothing; an empty
  or oversized flow ID is refused, an unknown one ignored.
* Protocol: the four calls refuse extra fields (`user_id`, `emoji`), a missing
  or null flow ID; there is no call to trust a device directly.
* UI host: only the flow ID crosses to the core; an oversized one is refused
  before the core; the emoji and device name come back unchanged for the page
  to render as text.
* Frontend: every phase as fixed text; seven emoji with their names; the
  device name escaped and isolated; Accept and Decline in an `alertdialog`; no
  second flow offered while one is open; a new attempt after a cancel.

## Live runs

The test peer ([`p4_peer`](../../tests/matrix/src/bin/p4_peer.rs)) signed in
as the first account on a device of its own and restored the cross-signing
keys with the recovery key that Persephone set up (written to a file under
`.local` by the development credential source). Persephone ran through the
real page (development core), one new device per step, driven with DevTools.

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| A: first device sets up encryption | verified | verified |
| B: Persephone asks, the peer accepts | same 7 emoji; both confirm; page says verified | same |
| C: the peer asks, Persephone accepts | same 7 emoji; both confirm; page says verified | same |
| D: Persephone says the emoji differ | cancelled on both sides; device still not verified | same |
| Exit codes (counted runs) | 0 | 0 |

Raw reports are in `.local/runs/p4c-04` (both servers on one harness start):
Synapse passed A to D in one run; Merovingian passed A and B, then C and D on
later attempts with the same accounts.

### Findings

* **The SDK drops a request from a device it does not know yet.** When a
  request arrives, `matrix-sdk-crypto` looks up the sender's device and ignores
  the request if it is missing. In the first runs the peer synced only while it
  ran a command, so its first sync after Persephone signed in carried both the
  device-list change and the request; the SDK handles to-device events before
  the keys query that the change starts, and the request was dropped (seen in
  Synapse's access log: one request sent, no reply). The peer now waits until it
  knows the new device (`wait-new-device`), as an always-syncing client would.
  A Persephone device can hit the same case: a request sent from a brand-new
  device while Persephone is locked is lost, and the user starts again.
* **A request to "my other devices" reaches only cross-signed devices.** A new
  Persephone device is not cross-signed, so the peer asks it by device ID
  (`Device::request_verification`), as a client does from its list of
  sessions.

### Attempts not counted

* Ten attempts (`.local/runs/p4c-02` to `p4c-04`) lost the DevTools page
  connection part-way. Five showed the core's `WebViewGone` detection, and the
  core then waited for the user to answer its retry offer, so the probe killed
  it. Five had no detection and the core exited with 0; their cause is not
  known. A control run in a signed-out window failed with `WebViewGone` while
  it showed **plain text only**, before any emoji, so the emoji grid is not the
  cause. MSI Afterburner and RTSS were running, as in the
  [window retry evidence](window-retry.md), and another project's server tests
  were running in WSL; no crash dump was checked this time, so neither is shown
  to be the cause.
* One Merovingian step A passed but its core did not exit within 60 s and was
  killed, with no window failure. On the next harness start step A exited
  with 0 on both servers.
* One Merovingian attempt of step C, right after a step B attempt whose window
  failed mid-flow, never showed the incoming request (no window failure; the
  core exited with 0). The next attempt passed. The cause is not known.
* Earlier runs used the peer without `wait-new-device`, and step B failed as
  described above. A rerun of step A on the same accounts lost its window and
  could not count anyway: the account already had an identity, and the probe
  had removed the old recovery-key file.

## Limits

* Verifying other people, and approving a contact's changed identity, are not
  built; requests from other users are ignored.
* The other device was an SDK peer, not another Matrix client; interoperability
  with Element or other clients is not checked.
* A request lost because Persephone did not know the sender's device is not
  reported to the user (the SDK only logs it).
* Debug builds on one Windows host only. The native password prompt and
  recovery-key window from the encryption setup are still not checked by hand.
