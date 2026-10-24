# Encrypted text: unclear delivery, new devices, withheld keys and recovery

Checked 23 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. Last part of the remaining P4 work (PR F): live
runs for the P4 exit evidence that earlier parts covered only by unit tests or
not at all. No product code changed; the test peer gained two commands.
**Result: on both servers, a message whose reply was lost read as "delivery
unknown", and its retry reached the contact exactly once; a new device the
contact had not signed got no key until the contact signed it; a new,
unverified Persephone device was shown that a key was withheld from it; and
a clean profile verified with the recovery key read earlier history from the
key backup.**

## Method

The driver (`.local/runs/p4d-driver/drive_f.py`) ran the real page through
WebView2's DevTools port, with the [test peer](../../tests/matrix/src/bin/p4_peer.rs)
as Bob in an encrypted direct-message room, set up as for
[contact verification](contact-verification.md). Persephone reached the
homeserver through a local TCP proxy in the driver. On request, the proxy
forwards one message send (`PUT …/send/…`), waits a second so the server can
store it, then closes the connection without passing the reply on.

| Step | What it checks | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- | --- |
| F1 | Unclear delivery: the reply to a send is cut | page: "Delivery unknown"; Retry: sent; Bob sees the text once | same |
| F2 | New device: Bob signs in a second device his identity has not signed | it cannot read the next message (`withheld: m.unverified`); Bob's first device can | same |
| F2 | Bob signs that device | it reads the next message; the earlier ones stay withheld from it | same |
| F3 | Bob writes while Persephone's first device is open | the device reads it | same |
| F4 | A clean profile signs in: a new, unverified device | Bob's next message shows as `withheld`; the earlier history cannot be read | same |
| F4 | Settings, "Use recovery key" | device verified; the earlier message is read from the key backup; so is the one withheld from it (below) | same |
| F4 | Send from the recovered profile | Bob reads it | same |
| Driver result | | pass (`.local/runs/p4f-02`) | pass (`.local/runs/p4f-01`) |

### Findings

* **A key backup can open a message withheld from a device.** After the clean
  profile was verified, the message Bob had withheld from it also became
  readable. Bob's client kept using the same encryption session, and
  Persephone's first device had backed that session up from an earlier
  message; a session's key also opens its later messages. Withholding keys
  from an unverified device therefore protects only against a device that
  cannot reach the account's key backup.
* **The retry reused its transaction ID**: the server stored the first
  attempt, answered the retry with the same event, and Bob's client saw one
  message. Without the cut reply, F1 would not distinguish this from a resend.

### Attempts not counted

* `.local/runs/p4f-01`, Merovingian: the DevTools connection was lost before
  the first step (MSI Afterburner and RTSS were running, as in earlier runs);
  the next harness start (`p4f-02`) passed.

## Limits

* The other side was an SDK peer, not another Matrix client such as Element.
* The proxy cut the reply to one send only. A send whose request never
  reached the server, and a lost reply to the retry, were not run; both are
  covered by unit tests of the outbox.
* Messages withheld from a device are shown as a notice; whether a later key
  backup can open them depends on the sender's session (above).
* Debug builds on one Windows host only.
