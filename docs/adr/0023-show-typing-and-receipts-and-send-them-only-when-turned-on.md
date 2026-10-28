# Show typing notices and read receipts, and send them only when turned on

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for typing notices and
read receipts to be shown, and sent only when the user turns them on.

## Context and Problem Statement

Typing notices (`m.typing`) and public read receipts (`m.read`) tell the
homeserver, every server in the room and every member when this account is
active and how far it has read. For high-risk users that is metadata worth
withholding. Persephone received neither: the Sliding Sync typing and
receipt extensions were turned off. Without any receipt, though, unread
counts never clear on the homeserver, so the account's other devices keep
counting messages already read. What does Persephone show, and what does it
send?

## Decision Drivers

* Send no activity metadata to other people unless the user chooses to.
* Keep unread counts correct on all of the account's devices.
* Show other people's typing and reading, which they chose to send.
* Keep received names inert and every list bounded.

## Considered Options

* Send nothing and show nothing (the former behaviour).
* Send and show everything, like most clients.
* Show other people's; send this account's typing notices and public
  receipts only when a setting is on; always send the private markers.

## Decision Outcome

Proposed option: "show other people's; send this account's only when a
setting is on; always send the private markers".

* Two account settings, both off by default and kept in the encrypted
  profile: "Tell people when you are typing" (`sendTyping`) and "Show people
  how far you have read" (`sendReadReceipts`).
* When a room's newest event has been seen (the window has focus and the
  timeline is at its end), `mark_read` sets the fully-read marker
  (`m.fully_read`, account data) and a private receipt (`m.read.private`),
  which only this account sees; a public `m.read` receipt is added only when
  `sendReadReceipts` is on. The core reads the setting itself, so the page
  cannot send a public receipt while it is off.
* `set_typing` is refused as `rejected` while `sendTyping` is off. While it
  is on, the page sends "typing" at most every four seconds while the user
  types, and "stopped" after five quiet seconds or when the message is sent.
* The Sliding Sync typing and receipt extensions are on, so other people's
  typing notices and receipts arrive. The core keeps the newest typing list
  of at most 64 rooms (11 users each) and reads receipts from the SDK's
  state store. `room_activity` returns at most ten projected user IDs each,
  this account left out.
* The page names up to two people typing, otherwise "Several people are
  typing…", and lists who else has read the newest event under the
  timeline. Every name is shown in its own `<bdi>`.

### Positive Consequences

* By default nothing tells other people when this account is active or
  reading, and unread counts still clear on every device of the account.
* Typing and reading by others are visible, as other clients show them.

### Negative Consequences

* Receiving typing notices and receipts makes each sync response larger, and
  the homeserver learns nothing new by it.
* The fully-read marker and the private receipt still tell this account's
  own homeserver what was read and when.
* A server that does not support private receipts may reject the request;
  the page treats marking as best effort and tries again with the next
  event.
* Tested with unit, protocol and IPC tests only; no live run against
  Merovingian or Synapse is recorded.

## Links

* Builds on the settings of [ADR-0022](0022-notify-through-push-rules-and-content-free-desktop-notifications.md).
