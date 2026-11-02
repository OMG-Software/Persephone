# Browse public room directories and join only listed rooms

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-11-02

Technical Story: on 2 November 2026 the owner asked for a public room
browser. The user enters the homeserver to browse; without one, their own
homeserver is browsed. The user can view a listed room and join it.

## Context and Problem Statement

Until now the page could join only a room it was invited to (ADR-0016). A
public room directory needs a join by room ID, for rooms on this homeserver
or another one. A join call that accepts any room ID would let a compromised
web view join any room the homeserver allows, which ADR-0004 tries to
prevent. How can the user browse directories and join public rooms without
giving the page a general join capability?

## Decision Drivers

* The web view is untrusted and keyless (ADR-0004); the core validates every
  request.
* Directory data is untrusted text from a homeserver or another server.
* Bound sizes before replies reach the page.
* No direct connection to a server other than the account's homeserver.
* Do not weaken ADR-0005: public rooms are usually unencrypted.

## Considered Options

* A `join_room` call that accepts any room ID or address.
* List directory pages in the core, and join only a room a page of this
  session listed as open to anyone.

## Decision Outcome

Proposed option: "list directory pages in the core, and join only a room a
page of this session listed as open to anyone".

* `public_rooms` lists one page (at most 30 rooms) of the account's
  homeserver's directory, or of a named server's directory, which the
  homeserver asks over federation (`POST /publicRooms?server=`). Nothing
  connects to the other server directly. An optional filter (at most 256
  bytes) narrows the page; both servers see it. Nothing is sent until the
  user presses Browse.
* The core projects names like names and topics like message bodies, cuts
  topics to 500 characters, and sends room IDs and addresses only when they
  are valid visible ASCII. It leaves out spaces and avatars, and sends a page
  token only when it is visible ASCII of at most 512 bytes. The reply must
  fit the bounded view size.
* The core remembers the rooms that pages of this session listed with the
  public join rule, at most 330 (a full 300-room listing and one more page),
  oldest forgotten first, with the server whose directory listed them. A new
  session forgets them all, and a page from an older session is ignored.
* A refusal (a 4xx answer) is `rejected`. A rate limit (429), a server
  error (5xx) or no answer at all is `unavailable`.
* `join_public_room` takes only a room ID. The core joins it through the
  server that listed it, and only when it is remembered; any other room is
  `not_found`. Knock and restricted rooms are shown but not joined.
* Sending in a joined unencrypted room still needs the user's consent
  (ADR-0005). The page says so before Join.

### Positive Consequences

* The user can find and join public rooms on any server that shares its
  directory.
* The page cannot choose the servers a join goes through, join by address,
  or join a room that no directory listed in this session.

### Negative Consequences

* Many homeservers refuse federated directory requests. Those directories
  cannot be browsed; the page says so.
* The homeserver sees which servers the user browses and what they search
  for, and the other server sees the search text.
* Joining reveals the user's Matrix ID to the room and its servers.
* A room cannot be joined by address alone, and history of a room that is
  readable without joining is not previewed.
* The join limit is weak. The page chooses the directory server, and a
  hostile server can list any room as open to anyone. A compromised web view
  can therefore still join any room the homeserver lets this account join,
  through such a server. A join confirmed in the core's own window, as for
  links (ADR-0024), would close this gap; it is an open follow-up.
* The directory reply is read whole before the core keeps 30 rooms. A server
  the page names can now send that reply, as the homeserver relays it.
* Names, addresses and topics are the directory's claim. A room can show an
  address it does not own; the page says so.
* Not yet checked against a live federated homeserver.

## Links

* Builds on [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Keeps [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
* Extends the joins of [ADR-0016](0016-create-encrypted-rooms-and-answer-every-invitation.md)
