# Bound room navigation and inspection in the core

* Status: proposed
* Deciders: project owner (technical design decision pending)
* Date: 2026-11-01

Technical Story: the owner requested date navigation, shared pins, attachment
browsing, local search filters and connection health on 1 November 2026.

## Context and Problem Statement

Room navigation and inspection can expose history, event references and
connection metadata. What authority and resource limits should these tools
use while keeping the web view keyless?

## Decision Drivers

* Keep Matrix, SDK, media and session authority in `crates/core`.
* Keep the Tauri host and web view keyless, with typed allowlisted requests.
* Tell users what data goes to the homeserver and when coverage is partial.
* Bound history work, response size and retained UI items.

## Considered Options

* Give the web view direct Matrix or media access.
* Put each feature in separate unbounded core operations.
* Use bounded typed operations in the core, with explicit disclosure and
  partial-coverage states.

## Decision Outcome

Proposed option: "bounded typed operations in the core". The owner approved
the feature requirements; this technical design remains proposed.

### Proposed rules

* **Date navigation:** send room ID and timestamp only. Request `since`; use
  `until` only after HTTP 404. Do not send a search term or scan history as a
  fallback. Return bounded context of no more than 100 timeline items and a
  gap marker at its newer end. The page cursor continues before the oldest
  retained item. Suppress read markers while focused on date or pin context.
  On room invalidation, cancel context work and clear the focused timeline and
  received reply quotes. Preserve authored draft and edit text. Show **Room
  changed. Return to latest to refresh.** Return to latest loads the live view.
* **Pins:** use standard, unencrypted `m.room.pinned_events` state with no
  plaintext snippet. Read fresh state and check current power before writes.
  Show at most 100 IDs and reject additions at that limit. Permit removal from
  existing lists within 512 IDs and a 112 KiB replacement-body limit, retaining
  all other references. Pin reads have a 10-second deadline. Read replacement
  responses in bounded chunks up to 256 KiB and reject more than 20 events
  before SDK decryption. Preserve one SDK token refresh and bounded retry.
  Equal edit timestamps use event ID ordering. This is not a global SDK
  response cap or drain guarantee. State replacement has no compare-and-swap
  guarantee.
* **Attachments:** scan one history chunk of at most 50 raw events per page.
  Use opaque room-scoped cursors. Filter kind, exact sender and date locally.
  Return metadata only; keep at most 500 items in the client. State that gaps,
  undecryptable events and byte limits make coverage partial.
* **Search filters:** require the existing query and support exact sender,
  inclusive start and exclusive end on the original message timestamp,
  including after edit or redaction reconstruction. Add no HTTP index or cache
  growth. Keep coverage limits explicit.
* **Health:** show a manual snapshot of existing sync metrics. Add no probe or
  background task. Label timestamps as local clock observations, not proof of
  a live connection. Reset on lock and reject stale generations.

Validate all inputs and bounds in the core. Use typed allowlisted IPC. Keep
media downloads behind existing explicit user actions. Treat server responses,
event metadata and UI requests as untrusted.

### Positive Consequences

* Matrix and media authority remain in the core.
* Network disclosures and incomplete coverage are visible and bounded.
* No new persistent search index or health polling is required.

### Negative Consequences

* Date navigation and attachment browsing disclose room/time or history
  requests to the homeserver.
* Shared pins expose event IDs to room participants and can race with other
  state writers.
* Partial history and cache contents can omit results.

## Links

* [Room tools](../room-tools.md)
* [SDK lifecycle contract](../architecture/sdk-lifecycle.md)
* [Verification record](../evidence/room-navigation-media-health.md)
