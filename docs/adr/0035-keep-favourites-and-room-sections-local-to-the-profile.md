# Keep favourites and room sections local to the profile

* Status: proposed
* Deciders: project owner (technical design decision pending)
* Date: 2026-11-01

Technical Story: the owner requested favourites and named room sections on
1 November 2026. The features are approved; the storage design remains proposed.

## Context and Problem Statement

Room organisation needs a persistence policy. Matrix room tags synchronise
through the homeserver, while browser storage creates another local copy.
How can a profile keep its own bounded room layout?

## Decision Drivers

* Keep store authority in the core and avoid browser persistence.
* Avoid uploading local organisation choices.
* Bound room and section counts and expose failed saves.

## Considered Options

* Matrix `m.tag` account data.
* Browser storage.
* One bounded preferences record in the encrypted crypto store.

## Decision Outcome

Proposed option: "one bounded preferences record in the encrypted crypto store".

Each entry has a valid Matrix room ID, a favourite flag and an optional section.
The core trims section names and requires 1 to 64 UTF-8 bytes with no controls.
It accepts at most 100 room entries, 16 distinct sections and a 112 KiB record.
Clearing both the favourite flag and section removes the entry and frees its
slot. A mutation gate serializes read-and-replace operations. Invalid stored
records and store errors fail closed without replacement.

The UI renders names as inert text and does not persist another copy. Choices
are local to one encrypted profile and are not uploaded as Matrix tags. Quick
room switching and room mute are existing, separate controls.

### Positive Consequences

* Organisation follows the profile's encryption and deletion policy.
* The homeserver receives no new room-tag data from these controls.

### Negative Consequences

* Choices do not synchronise to another device.
* Limits can refuse new entries; save failures need clear UI feedback.
* Reopen, isolation, corruption, limit and UI checks remain required.

## Links

* [Room tools](../room-tools.md)
* [Verification record](../evidence/room-tools.md)
* [Local profile protection](0006-require-an-unlock-secret-for-the-local-profile.md)
