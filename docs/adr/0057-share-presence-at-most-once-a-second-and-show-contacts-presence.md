# Share presence at most once a second and show contacts' presence

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-11-21

Technical Story: on 21 November 2026 the owner reported that Persephone and
another client of the same account set the account's presence in turn and
flood the homeserver. The owner asked for presence to be set and shown
without flooding, at most once a second, shared as online by default, with a
setting to turn sharing off.

## Context and Problem Statement

Persephone made no presence call of its own, but matrix-sdk 0.19 sends
`set_presence=online` with every classic `/sync` and Sliding Sync request
unless the client says otherwise. The sync poll is 5 seconds. With classic
sync, a presence change by another client of the account ends Persephone's
long poll at once, and its next request sets `online` again, which ends the
other client's poll. Two clients that want different states then write the
presence as fast as the homeserver answers. A homeserver that combines the
presence of each device can absorb this; one that keeps a single state for
the account cannot. Sliding Sync (MSC4186) has no presence extension, so
Persephone did not receive any presence there.

The development plan defaulted to no online-presence announcement. The SDK
default broke that without any app call, which the plan warned can happen.

How does Persephone set its presence without flooding, and how does it show
other people's?

## Decision Drivers

* At most one request a second may change the account's presence.
* Do not fight another client of the account.
* Keep presence alive on homeservers that time out a user who stops
  syncing, such as Synapse after 30 seconds.
* Show presence with Sliding Sync too, with bounded requests.
* Let the user stop sharing presence.

## Considered Options

* Keep the SDK default: every sync request sends `online`.
* Send presence only with `PUT /presence/{userId}/status`, every sync request
  sending `offline`, which leaves the presence as it is.
* Let the sync requests carry the chosen presence at most every 20 seconds,
  send each change with one `PUT`, and let no two of these writes come within
  a second of each other.

## Decision Outcome

Proposed option: "carry the presence at most every 20 seconds, `PUT` each
change, at most one write a second", because it bounds the writes, keeps a
fight between two clients to one change in 20 seconds, and keeps the presence
alive on Synapse without a separate timer.

* Two account settings in the encrypted profile: "Share your presence"
  (`sharePresence`, on by default) and "Your presence" (`presence`: online,
  away or appear offline; online by default). The owner chose these defaults
  on 21 November 2026; they replace the development plan's "no presence"
  default for presence only.
* `presence::Writer` in the core picks the `set_presence` value of every sync
  request: the chosen state when sharing is on, the state is not offline, the
  last presence write was at least 1 second ago and the last sync request
  that carried it at least 20 seconds ago; otherwise `offline`, which the
  client-server API defines as not marking the user online. Classic sync gets
  the value per request; Sliding Sync reads it from the client.
* A changed setting waits for one `PUT`, which the sync loop sends before its
  next round, under the same 1-second limit. The newest change wins, and a
  failed or timed-out `PUT` waits for the next round. "Appear offline" is sent
  by its `PUT` alone, also when a session starts. Turning sharing off sends
  nothing more.
* `presence::Book` keeps the presence of at most 32 direct-message contacts.
  Classic sync fills it from `m.presence` events, and from the SDK's store
  for a contact first seen after its event arrived. With Sliding Sync, the sync
  loop asks `GET /presence/{userId}/status` for at most 2 contacts a round
  (5 seconds each at most), each at most once in 5 minutes, or once in a
  minute after the page opens the contact's room. A failed lookup keeps the
  old value. A lock clears the book.
* `RoomSummary.contactPresence` carries a contact's state to the page. The
  room list shows a labelled dot for online, away and offline, and nothing
  for an unknown state.

### Positive Consequences

* No request sequence can change the presence more than once a second.
* A second client of the account that wants another state can no longer
  start a flood; at most one change in 20 seconds comes from Persephone.
* Sharing can be turned off. Persephone then never changes the presence.

### Negative Consequences

* Presence is shared by default, so other people can see when the account
  is online. The account's homeserver, the servers of rooms it shares, and
  their members learn this.
* A changed setting reaches the homeserver at the start of the next sync
  round, up to about 6 seconds later.
* "Appear offline" is not kept against other clients: another client that
  says online wins until it stops.
* The 20-second carry relies on Synapse's 30-second sync timeout. A
  homeserver with a shorter timeout may show the account offline between
  carried requests. Merovingian's behaviour is not measured.
* Lookups with Sliding Sync tell the homeserver which contacts' presence the
  account asks for, and add up to 2 requests a round.
* Tested with unit, protocol, IPC and frontend tests only. No live run
  against Merovingian or Synapse is recorded.

## Links

* Builds on the account settings of [ADR-0022](0022-notify-through-push-rules-and-content-free-desktop-notifications.md)
  and follows [ADR-0023](0023-show-typing-and-receipts-and-send-them-only-when-turned-on.md).
* Matrix client-server API: `GET /sync` (`set_presence`), `PUT` and `GET
  /presence/{userId}/status`.
