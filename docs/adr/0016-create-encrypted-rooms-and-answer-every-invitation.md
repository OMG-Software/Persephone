# Create encrypted rooms and answer every invitation

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-27

Technical Story: on 26 October 2026 the owner reported that an invitation
sent over federation by another user never appeared, and asked for a way to
create a room and invite people to it.

## Context and Problem Statement

ADR-0014 listed only invitations marked direct: the core checked the
`is_direct` flag of the invitation's member event. Invitations to ordinary
rooms, and direct invitations from clients that do not set the flag, were
dropped without a trace. An invitation whose stripped state lacked the
member event was dropped as well. Persephone also had no way to create a
room other than a direct-message one. How can the user answer every
invitation and create rooms without weakening ADR-0005?

## Decision Drivers

* Every room Persephone creates must be encrypted from its first state event.
* Room keys go only to cross-signed devices (ADR-0005).
* Names and IDs from any homeserver are untrusted text.
* Use SDK-owned room creation and invitation; no server-specific shortcuts.

## Considered Options

* Keep direct invitations only (ADR-0014).
* List every invitation, and create invite-only encrypted rooms.

## Decision Outcome

Proposed option: "list every invitation, and create invite-only encrypted
rooms".

* `invite_list` lists every invited room, direct or not, with the room's name
  and whether it is direct. An invitation whose inviter cannot be read is
  still listed, without an inviter. Accept joins and Decline leaves, as
  before.
* `create_room` creates a room with `m.room.encryption` in its initial state
  and the private chat preset (invite-only; invitees are not made
  administrators). It invites at most 10 people. If one of them has no
  cross-signing identity, nothing is created.
* `invite_user` invites one person to a joined room. In a room that is not
  known to be unencrypted, the person must have a cross-signing identity.
  The UI offers it in rooms that are not direct-message rooms.

### Positive Consequences

* Invitations from other homeservers and other clients reach the user.
* Group rooms can be created and grown from Persephone.

### Negative Consequences

* Joining a room created elsewhere can join an unencrypted room. Sending
  there still needs the user's consent (ADR-0005).
* The inviter's name and the room name are the invitation's word and may
  mislead.
* Not yet checked against a live federated homeserver.

## Links

* Amends the invitation rule of [ADR-0014](0014-start-encrypted-direct-conversations-by-matrix-id.md)
* Builds on [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
