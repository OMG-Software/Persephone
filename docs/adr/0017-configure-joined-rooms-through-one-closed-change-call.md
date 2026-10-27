# Configure joined rooms through one closed change call

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-27

Technical Story: on 27 October 2026 the owner asked for room settings:
"anything that can be configured for a room should be configurable",
including the name, addresses, visibility, members and their power levels.

## Context and Problem Statement

Persephone could create a room and invite people (ADR-0016) but could not
read or change anything else about a room. A room has many settings, each a
state event or a homeserver call, and acting on members needs power levels.
How can the keyless UI (ADR-0004) change them without gaining a generic
state-event or homeserver capability, and without weakening ADR-0005?

## Decision Drivers

* No generic capability crosses to the page: no arbitrary event type,
  content or endpoint.
* Every argument is validated in the core; replies stay bounded.
* Encryption can be turned on, never off.
* Changes must not silently drop room state this client does not show.
* Use SDK-owned room operations; no server-specific shortcuts.

## Considered Options

* One command per setting (about twenty Tauri commands and permissions).
* A generic "send state event" command.
* Two bounded read calls and one change call whose argument is a closed,
  tagged set of changes.

## Decision Outcome

Proposed option: "two bounded read calls and one change call", because it
keeps the page's surface small and explicit without a generic capability.

* `room_settings` reads a joined room: name, topic, main and published
  addresses, local addresses and the directory listing (both asked of the
  homeserver; unknown when it does not answer), join rule, history
  visibility, guest access, encryption, notification mode, room version,
  every power level Persephone shows, this account's level, and what it may
  change by the SDK's power-level rules. The permissions are hints; the
  homeserver decides.
* `room_members` lists joined, invited, asking and banned members that match
  a filter, at most 100 and within the view budget, with per-member hints for
  remove, ban, unban and changing the level.
* `change_room` takes one `RoomChange` from a closed set: name, topic, join
  rule (invite, knock or public only), history visibility, guest access,
  turn on encryption, directory listing, add or remove a local address, main
  and published addresses, one power level, one member's level, remove, ban,
  unban, notification mode, and leave. Unknown kinds and fields fail to
  decode at the Tauri boundary and in the core.
* A level change reads the latest synced power levels and replaces only the
  named entry. Published addresses the page was not shown are kept. A
  creator's unlimited level (room version 12) is not changed. Removing an
  address checks that it leads to this room first.
* The page asks before leaving, removing, banning, turning on encryption,
  making the room public, deleting an address, and lowering this account's
  own level or giving someone else the same level.
* Not configurable here: the room picture (it would need media upload and
  display through ADR-0009), room upgrades (they create a new room), server
  access lists, restricted (space-based) join rules, and retention.

### Positive Consequences

* Rooms can be configured and moderated from Persephone.
* The UI gains three explicit commands, not a state-event capability.

### Negative Consequences

* Most settings are read from the device's synced state, so a change shows
  in a fresh read only after the next sync; the page shows it at once
  locally and reads again on the room's next change notification.
* Notification modes are kept on the homeserver, but Persephone does not show
  notifications yet; they apply to the account's other devices.
* Making an encrypted room public lets anyone who joins receive keys for new
  messages; the page warns, the homeserver allows it.
* Not yet checked against a live Merovingian or Synapse homeserver.

## Links

* Builds on [ADR-0016](0016-create-encrypted-rooms-and-answer-every-invitation.md)
* Keeps [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
  and [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
