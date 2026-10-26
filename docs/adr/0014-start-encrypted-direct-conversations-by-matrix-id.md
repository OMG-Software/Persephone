# Start encrypted direct conversations by Matrix ID

* Status: accepted
* Deciders: project owner
* Date: 2026-10-26

Technical Story: on 26 October 2026 the owner noted that Persephone had no way
to start a conversation with someone, and asked for one. Persephone also could
not see or accept invitations, so two Persephone users could never talk.

## Context and Problem Statement

The client could open only rooms the account had already joined. Contact
verification (`crates/core/src/contacts.rs`) deliberately never creates or
invites. How does a user start a direct conversation, and answer one, without
weakening the encryption policy (ADR-0005) or telling the homeserver more than
needed?

## Decision Drivers

* Every conversation Persephone starts must be end-to-end encrypted; there is
  no plaintext variant to fall back to.
* Room keys go only to cross-signed devices (ADR-0005). A person without a
  cross-signing identity could not read anything sent to them.
* User-directory search sends every term to the homeserver: metadata about
  whom the user is looking for.
* Use SDK-owned room creation; no server-specific shortcuts.

## Considered Options

* Exact Matrix ID only.
* Exact Matrix ID, with user-directory search behind a switch that is off by
  default.
* Directory search always on.

## Decision Outcome

Chosen option: "Exact Matrix ID, with user-directory search behind a switch
that is off by default", the owner's decision of 26 October 2026, together
with these owner decisions from the same day:

* Starting a conversation uses the SDK's `Client::create_dm`: encryption in
  the room's initial state, the trusted private chat preset, the person
  invited and the room marked direct. An existing direct-message room with
  just that person is opened instead of creating another.
* The core refuses to start a conversation with someone who has no
  cross-signing identity (`no_identity`), after asking the homeserver for
  their keys. Nothing is created.
* Incoming direct-message invitations are listed and can be accepted (join)
  or declined (leave). Other invitations are not shown.
* The switch is not remembered: it is off each time the dialog opens.
  Results show names and IDs as text; no avatars are fetched.

### Positive Consequences

* Two Persephone users can start and accept encrypted conversations.
* By default nothing about a search reaches the homeserver until the user
  presses Start with a full Matrix ID.

### Negative Consequences

* A user must know the exact Matrix ID unless they turn search on.
* People without cross-signing cannot be reached from Persephone until they
  set it up, and the message cannot tell that case from a Matrix ID that does
  not exist (the homeserver answers both with no keys).
* Group-room invitations stay invisible; they need another client.
* Starting a conversation reveals the invitation to both homeservers, as any
  Matrix invitation does.

## Links

* Builds on [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
* Builds on [ADR-0002](0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md)
