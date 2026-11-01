# Ignore users and report events through the homeserver

* Status: proposed
* Deciders: project owner (technical design decision pending)
* Date: 2026-11-01

Technical Story: the owner requested ignore and report controls on 1 November
2026. The features are approved; their technical design remains proposed.

## Context and Problem Statement

Users need to hide unwanted messages and request moderation. These operations
have different effects and disclosure rules. Ignoring cannot revoke room
access, and a report reason can disclose private text from an encrypted room.

## Decision Drivers

* Standard Matrix behavior across homeservers and clients.
* Complete filtering even when a displayed list is bounded.
* Explicit disclosure before a report leaves the device.

## Considered Options

* Local-only ignore records and no reporting.
* A project-operated moderation service.
* Standard Matrix ignore account data and reports to the account's homeserver.

## Decision Outcome

Proposed option: "standard Matrix ignore account data and reports to the
account's homeserver".

The core uses SDK account-data APIs to ignore or unignore a valid Matrix user.
It updates local state only after server success. Writes are serialized to
avoid lost updates. A new ignore is refused once the set has 100 users; a
larger existing set can still be reduced. Message views and search filter the
full set, not the truncated display list. State events can remain visible.
Raw account data above 256 KiB is refused rather than treated as an empty list.
Ignoring does not remove membership, revoke room access or withdraw shared keys.

A report requires a nonempty reason of at most 1,000 UTF-8 bytes and an explicit
UI confirmation. The core validates the joined room and event reference. It
sends only the room ID, event ID and reason to the account's own homeserver,
without attaching a message body. The reason is readable by the operator even
for an encrypted room. Failures must remain visible.

### Positive Consequences

* Other clients can use the account's standard ignore list.
* No additional moderation service receives the report.

### Negative Consequences

* The homeserver can read the ignore list and report reason.
* Reports depend on server support and moderation policy.
* The UI confirmation is an intent control, not protection from a compromised
  UI that can misuse allowed commands. Live interoperability checks remain open.

## Links

* [Room tools](../room-tools.md)
* [Privacy notice](../../PRIVACY.md)
* [Verification record](../evidence/room-tools.md)
