# Search only the existing encrypted event cache

* Status: proposed
* Deciders: project owner (technical design decision pending)
* Date: 2026-11-01

Technical Story: the owner requested local message search on 1 November 2026.
The feature is approved; the technical design remains proposed.

## Context and Problem Statement

Search can disclose queries to a server or create another persisted copy of
decrypted history. The app already has an encrypted SDK event cache. How can
it search that cache without implying complete history coverage?

## Decision Drivers

* No query disclosure, history download or room-key request from search.
* No new plaintext index or duplicate history database.
* Bounded work and explicit partial coverage.

## Considered Options

* Homeserver search.
* A separate local full-text index.
* Bounded substring search of existing encrypted SDK cache chunks.

## Decision Outcome

Proposed option: "bounded substring search of existing encrypted SDK cache chunks".

The core reads retained chunks for joined rooms and searches only already-
decrypted text, notice and emote messages. It applies available edits and
redactions and excludes ignored senders using the full ignore set. It makes
no HTTP request, paginates no history and asks for no keys. The UI shows partial
coverage even when no explicit limit is reached. Missing cache events can mean
missing messages or later edits and redactions.

Separate chunk reads are not an atomic cache snapshot. Use the greatest
`origin_server_ts`, then greatest event ID, for available valid edits, as
specified by the [Matrix replacement rules](https://spec.matrix.org/latest/client-server-api/#event-replacements).
Omit a room if an oversized cached event prevents checking its relations.
Selecting a result is a separate navigation action that can request normal
history; the search term is never part of that request.

Bound each query to 100 rooms, 2,000 scanned events, 50 matches, 512-byte UTF-8
snippets and one 112 KiB reply. Further chunk and scan-byte limits constrain
work. The SDK cache subscribes before sync so sync can persist events. That
subscription adds background work to the SDK lifecycle inventory. It does not
establish full stop-and-join ownership; `restart_required` remains.

### Positive Consequences

* Search itself discloses no query to the homeserver.
* No second search index needs encryption, migration or deletion policy.

### Negative Consequences

* Search cannot establish that an absent result does not exist.
* Old cache coverage and missing keys limit matches after restart.
* Cache persistence and no-request behavior need direct verification, including
  observable controls. Bounded scan tests alone do not prove either property.

## Links

* [Room tools](../room-tools.md)
* [Verification record](../evidence/room-tools.md)
* [SDK lifecycle contract](../architecture/sdk-lifecycle.md)
