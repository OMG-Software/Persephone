# Keep bounded room drafts in the encrypted profile

* Status: proposed
* Deciders: project owner (technical design decision pending)
* Date: 2026-11-01

Technical Story: the owner requested per-room drafts on 1 November 2026. The
feature is approved; this record does not assume approval of its technical design.

## Context and Problem Statement

Changing rooms or restarting should not silently lose composed text and its
reply reference. Browser persistence would put a second plaintext copy outside
the key-holding core. How can the app keep drafts with bounded storage and
clear failure behavior?

## Decision Drivers

* Keep store and key authority in the core under ADR-0004.
* Preserve text across room changes without mixing rooms or profiles.
* Bound records and refuse corruption without overwriting data.

## Considered Options

* In-memory drafts only.
* Browser storage or a separate draft database.
* One bounded record in the SDK's encrypted crypto store.

## Decision Outcome

Proposed option: "one bounded record in the SDK's encrypted crypto store".

The core keeps text and an optional reply event ID, not pending media or queued
sends. It accepts a 16 KiB UTF-8 body, valid Matrix IDs of at most 255 bytes,
100 rooms, 100 KiB of aggregate bodies and a 112 KiB serialized record. Message
control policy applies, but empty and whitespace-only drafts are valid. An
empty body without a reply clears the entry and frees its slot. Invalid stored
records and read failures return an error; they never become defaults that a
save replaces. A mutation gate serializes each read-and-replace operation.

The intended UI debounces saves by 350 ms, serializes them, fences loads and
saves by room and session generation, and shows save failures. UI race checks
remain required. No draft is persisted in the web view.

### Positive Consequences

* Drafts follow the profile's existing encryption and deletion boundary.
* A small fixed key set and explicit limits constrain storage growth.

### Negative Consequences

* A crash before a save completes can lose unsaved text.
* Serialization overhead can reach the record limit before the body budget.
* This adds persisted user text to the storage-review scope. The complete SDK
  drain gap remains open; persistence does not prove safe store handoff.

## Links

* [Room tools](../room-tools.md)
* [Verification record](../evidence/room-tools.md)
* [Local profile protection](0006-require-an-unlock-secret-for-the-local-profile.md)
