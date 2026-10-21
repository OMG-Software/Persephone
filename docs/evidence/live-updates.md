# Live room updates from the core (synthetic data)

Checked 20 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
WebView2. P1 "core events": the core tells the UI when a subscribed room
changes, and the page fetches the change through the existing bounded
[timeline pages](mock-views.md). **Result: events carry no content and
coalesce, so neither process queues more than one notification per subscribed
room, however fast rooms change or however slowly the page reads.** Messages
still come from synthetic data; `persephone --demo-live` posts one to the
planning room every 3 seconds, up to 1,000.

## Design

* **Protocol** ([`core-protocol`](../../crates/core-protocol/src/lib.rs)): the
  core now writes `CoreMessage::Reply` or `CoreMessage::Event` frames on the
  same pipe. The only event is `room_changed { room_id }`. New calls
  `subscribe { room_id }` and `unsubscribe { room_id }`; at most 16
  subscriptions (`limit_exceeded` after that), and only rooms the core knows.
* **Core** ([`lib.rs`](../../crates/core/src/lib.rs)): a change to a subscribed
  room adds the room to a pending set and wakes a notifier thread, which writes
  one event per pending room. Replies and events are written whole under one
  lock. Pending state is a set of at most 16 room IDs; 10,000 changes before
  the UI reads still leave one pending notification.
* **UI transport** ([`core_link.rs`](../../src-tauri/src/core_link.rs)): one
  reader thread routes replies to the waiting caller by request ID and passes
  events on. Calls may now run concurrently. A reply nobody is waiting for, or
  any read or write failure, breaks the channel for every caller.
* **UI to page** ([`lib.rs`](../../src-tauri/src/lib.rs)): `subscribe_room`
  takes the page's Tauri channel and stores it only after the core accepts the
  room. Each event becomes an empty (`null`) message on that room's channel,
  well under Tauri's 8 KiB direct-delivery threshold, so no
  `plugin:__TAURI_CHANNEL__|fetch` path is involved. A page load drops every
  channel and unsubscribes those rooms in the core. Both commands are in the
  allowlist and capability; the page sees only the existing error strings plus
  `limit_exceeded`.
* **Frontend** ([`App.svelte`](../../frontend/src/App.svelte),
  [`timeline.ts`](../../frontend/src/timeline.ts)): opening a room subscribes
  first and then fetches, so a change between the two is not missed.
  Subscribe and unsubscribe calls run through one queue, because the host runs
  commands concurrently and could otherwise reorder them. Changes during a
  fetch are applied after it. The newest page is merged by event ID; if more
  than a page arrived at once a gap marks the missing messages, and live
  updates keep at most 500 items, replacing the oldest with one gap.

## Tests

`python tools/dev.py check --offline` passed 186 tests (159 Rust, 12 frontend,
2 server-registration and 13 controller tests), with formatting, Clippy and
contract checks. New:

* Core (3): subscription rules (unknown room, malformed or oversized ID, the
  16-room limit, re-subscribing takes no extra slot, unsubscribing frees one);
  10,000 changes leave one pending notification and unsubscribing drops it;
  over real pipes, a change to a subscribed room produces an event, and the
  following newest-page fetch returns the new message.
* UI transport (2): replies answered out of order with an event between them
  reach the right callers; a reply nobody asked for, or the core closing,
  makes waiting and later calls `core_unavailable`.
* UI commands (1, mock runtime with a channel interceptor): the channel is
  stored only after the core accepts the room; `not_found`, `limit_exceeded`
  and `invalid_argument` are relayed; a channel value that is not a page
  channel is refused; events reach only the subscribed room's channel, as
  `null`; nothing arrives after unsubscribing.
* Frontend (4): merging appends only new messages, an empty timeline takes the
  page, a gap marks a skipped run, and the retained cap holds with one leading
  gap.

## Real application walkthrough

`persephone --demo-live`, driven over WebView2's DevTools port:

| Check | Result |
| --- | --- |
| Open the planning room | 100 items, last "Planning note 249" |
| Live messages | The first appeared 1.0 s after the room opened (posts come every 3 s); three in total after a further 6.5 s, no duplicates |
| Another room open | No live messages shown there |
| Back to planning | Newest page includes messages posted while away |
| Reload, reopen planning | Updates continue |
| Reload, planning **not** reopened | No messages reached the old page's channel in 8 s (control: a message to an unknown channel ID is caught by the same check) |
| Alerts | None |

Three earlier attempts at the last check lost the DevTools connection before
reaching it, the debugging-channel flakiness noted in the
[views evidence](mock-views.md#investigation-note).

## Limits

Synthetic data and a demo poster stand in for Matrix sync. Only one room (the
open one) is subscribed by the frontend. Events say only that a room changed;
room-list changes (new rooms, unread counts) are not sent yet. Release
builds, Linux/macOS and screen-reader announcement of new messages are not
covered.
