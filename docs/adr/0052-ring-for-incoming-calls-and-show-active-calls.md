# Ring for incoming calls and show active calls

* Status: proposed
* Deciders: project owner
* Date: 2026-11-19

Technical Story: on 19 November 2026 the owner asked for incoming call
ringing, as part of the next call work after
[ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
and [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md).

## Context and Problem Statement

Persephone can join a call, but it does not tell the user that somebody
calls them. The user sees a call only when they open the room. Persephone
also sends no signal when it starts a call, so other clients do not ring.
How does Persephone receive and send call rings, and how does it show
rooms that have a call?

Facts from the research of 19 November 2026 (matrix-js-sdk
`MatrixRTCSession.ts`, Element Call and Element Web source, the
ruma-events 0.35.1 and matrix-sdk 0.19.1 sources in the local Cargo
registry, and the Synapse 1.163.0 changelog). They are not verified
against a live Element client:

* Element Call and matrix-js-sdk send `org.matrix.msc4075.rtc.notification`.
  The older `org.matrix.msc4075.call.notify` is deprecated.
* The content has `notification_type` (`ring` or `notification`),
  `m.mentions`, `m.relates_to` (a reference to the sender's membership
  event), `sender_ts`, `lifetime` (90 s by default) and, optionally,
  `m.call.intent`. The first member of a new call sends it: `ring` in a
  direct chat, `notification` in other rooms.
* The decline is `org.matrix.msc4310.rtc.decline`, a reference to the
  notification. Element Web sends it; Element Call ends an outgoing direct
  chat ring when it receives one. MSC4310 was closed without a merge, and the
  open MSC4075 draft renames both events. The shipped clients still use the
  unstable names above.
* matrix-sdk 0.19.1 enables `unstable-msc4075` and `unstable-msc4310` in
  ruma. `RtcNotificationEventContent` and `RtcDeclineEventContent` exist.
  `unstable-msc4354` (sticky events) is not enabled.
* Synapse 1.163 has push rules for these events only behind the
  `msc4075_enabled` flag, which is off by default. A server cannot match a
  push rule on an encrypted event.
* Persephone already shows content-free desktop notifications through push
  rules ([ADR-0022](0022-notify-through-push-rules-and-content-free-desktop-notifications.md)).

## Decision Drivers

* Interoperate with Element Call, Element Web and Element X as they ship.
* Treat every ring as untrusted. A ring must not join a call, turn on a
  microphone or camera, or open a network connection.
* Bound the work and the state that remote events can cause.
* Keep the page keyless and the notification text content-free by default.

## Considered Options

* Ring from MSC4075 notifications, checked against call membership
* Ring from call membership events alone
* No ringing; show active calls only

## Decision Outcome

Chosen option: "Ring from MSC4075 notifications, checked against call
membership", because it is what the other clients send, and the membership
check stops rings that no call supports.

### Receiving a ring

* The core reads `org.matrix.msc4075.rtc.notification` room events after
  decryption. It reads no other ring event type.
* The core accepts a ring only when all of these are true:
  * the room is joined and is not the open call's room;
  * the sender is a joined member, is not this user and is not ignored;
  * the sender has a current call membership in the room, as
    `read_members` reports it (ADR-0043 rules: state key names the sender,
    `https://` service, not expired);
  * `notification_type` is `ring`;
  * the ring has not expired (see below).
* `notification` events do not ring. They mark the room as having a call,
  the same as a membership would.
* Expiry: the start is `sender_ts`, unless it is more than 20 s after
  `origin_server_ts`; then the start is `origin_server_ts`. The ring ends at
  start plus `lifetime`, with `lifetime` at most 120 s, and at most 120 s
  after the core received it. A ring that ends before it arrives is dropped.
  The `lifetime` cap stops an old ring from ringing again after a restart.
* A ring ends when it expires, when this user joins a call in that room,
  when this user declines it, when the sender's membership ends, or when the
  user leaves the room.
* Bounds: at most one ring for each room (a newer ring replaces the older
  one) and at most 4 rings in all (the oldest is dropped). At most one ring
  from each sender every 30 s; a sooner one is dropped.
* The core keeps rings in memory only. A restart, a lock or a sign-out
  clears them. Rings from the timeline that sync gives after a restart are
  checked again, so an old ring has expired by then.
* MSC4354 sticky delivery is not read; see "Negative Consequences".

### Showing a ring

* The core tells the page through a content-free, coalesced event. The page
  asks for the rings. Each ring has the room ID, the caller's validated user
  ID, the intent (`audio` or `video`; absent or unknown is `audio`) and the
  time left in whole seconds.
* The page shows a prompt with the room name, the caller's name, and three
  buttons: answer, answer with video (only when a camera is available), and
  decline. It uses the same inert text rules as the rest of the page.
* Answer is the existing join command for that room, with the camera on or
  off. The join runs every ADR-0043 check: readiness, the proxy warning and
  cross-signed keys. A ring gives no exception.
* The desktop notification uses the ADR-0022 path. The text is "Incoming
  call", with the caller's name only when the existing name setting allows
  names. It has no action buttons. A ring makes at most one desktop
  notification.
* There is no bundled ringtone in this version. The desktop notification
  uses the OS sound. The page shows the prompt until the ring ends.

### Declining

* Decline sends `org.matrix.msc4310.rtc.decline`, a reference to the
  notification event, as a normal room event. In an encrypted room it is
  encrypted like any other message. Then the core ends the ring.
* If the send fails, the core still ends the ring and does not retry.

### Sending a ring

* When Persephone joins a call and no other member is in it, it sends one
  `org.matrix.msc4075.rtc.notification` after its membership event is sent.
* `notification_type` is `ring` when the room is a direct chat with exactly
  one other joined member. Otherwise it is `notification`.
* The content is: `notification_type`, `m.mentions` with `room: true` for
  `notification` and the other member's user ID for `ring`, `m.relates_to`
  as a reference to the own membership event, `sender_ts` (the core's
  clock), `lifetime` 90000 and `m.call.intent` (`audio` or `video`).
* It is a normal room event. In an encrypted room it is encrypted. It is not
  a sticky event.
* A `notification` with a room mention needs the right to notify the room.
  When power levels do not allow it, the core sends nothing.
* A decline that references the own ring, from a member of a direct chat,
  makes the core show "Declined" in the call bar. The call goes on until the
  user leaves.
* A send failure does not end the call.

### Rooms with a call

* The room list summary gains a flag: some other device is in this room's
  call. The core keeps the set of rooms with such members, updated by the
  call membership state handler and by membership expiry. It keeps at most
  one flag for each joined room and no names.
* The page shows a call mark beside the room. The room header shows it as
  the call button already does with `active_members`.

### Positive Consequences

* Calls from Element clients ring in Persephone, and Persephone's calls ring
  in Element clients.
* A ring cannot start media, open a connection or bypass a join check.
* Remote events can create at most 4 rings and one flag per room.

### Negative Consequences

* Unstable event names. When MSC4075 settles on `m.rtc.invite` and
  `m.rtc.decline`, this record needs an update.
* Sticky (MSC4354) notifications are not read. A client that sends only a
  sticky event, with no plain fallback, does not ring Persephone. This needs
  a live check with Element Call against Synapse and Merovingian.
* Push rules for these events are off on most servers, and they cannot
  match encrypted events. So a ring raises a desktop notification only
  while Persephone syncs. There is no mobile-style wake-up.
* No ringtone, and the ring has no action buttons in the desktop
  notification.
* Not verified live.

## Pros and Cons of the Options

### Ring from MSC4075 notifications, checked against call membership

* Good, because Element clients send it.
* Good, because the membership check stops rings for calls that do not run.
* Bad, because the event names are unstable.

### Ring from call membership events alone

* Good, because it needs no extra event.
* Bad, because every new call in any room would ring, and a direct chat and
  a group look the same.

### No ringing; show active calls only

* Good, because it is the smallest change.
* Bad, because the owner asked for ringing, and a user who is not looking at
  the room misses the call.

## Links

* [ADR-0022](0022-notify-through-push-rules-and-content-free-desktop-notifications.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)
* [MSC4075](https://github.com/matrix-org/matrix-spec-proposals/pull/4075)
* [MSC4310](https://github.com/matrix-org/matrix-spec-proposals/pull/4310)
