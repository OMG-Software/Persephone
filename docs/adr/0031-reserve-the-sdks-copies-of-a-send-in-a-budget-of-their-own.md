# Reserve the SDK's copies of a send in a budget of their own

* Status: proposed
* Deciders: project owner (asked for the outcome; this form is proposed)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked to "raise the encoded
in-flight budget so images and files up to 20 MiB can be sent again; record
it in an ADR".

## Context and Problem Statement

The core reserves memory before it allocates (ADR-0004; the media
protocol's admission budgets). Sending an image or file through
matrix-sdk 0.19.1 (`Room::send_attachment`) holds up to six copies of it at
the peak: the outbox's; the SDK's input, kept for the whole call; the upload
buffer, which the request keeps for a retry after a token refresh (a clone
in a room that is not encrypted, or the encrypted copy, whose capacity can
reach twice the length as it grows); the clone of the request that is sent;
and the HTTP body serialized from it. Review of this change found that only
one or two were reserved. Counting all six in the 40 MiB `EncodedInFlight`
budget left room for a 6 MiB attachment, against 20 MiB before. How do
20 MiB sends fit, with every copy reserved?

## Decision Drivers

* Reserve every copy before it is allocated; never send past a budget.
* Send what can be received: 20 MiB, the encoded limit.
* A send must not starve image downloads, picks and drafts, nor they a
  send.
* Keep the contained worker's admission within its own memory limit.

## Considered Options

* Keep the 6 MiB send limit.
* Raise `EncodedInFlight` from 40 MiB to 160 MiB, shared with downloads.
* A class of its own, `OutgoingSend`, for the SDK's copies of one send.

## Decision Outcome

Proposed option: "a class of its own", because it sends 20 MiB with every
copy reserved, and downloads and sends cannot use each other's room.

* `OutgoingSend` is 100 MiB: the five SDK copies of one 20 MiB attachment.
  One attachment is sent at a time (the outbox holds one unsent image or
  file), so one send fills it.
* The outbox's copy stays in `EncodedInFlight` (40 MiB), as before, beside
  downloads, picks and drafts.
* The app's admission ceiling rises by the same 100 MiB, from 256 MiB to
  356 MiB, so the 56 MiB headroom is unchanged.
* `MAX_SENT_BYTES` equals `MAX_ENCODED_BYTES` (20 MiB). Picked files are
  checked against it on the open file before anything is read. Compile-time
  checks keep the send limit at least the read limit and the SDK copies
  within `OutgoingSend`.

### Positive Consequences

* Images (after stripping) and files up to 20 MiB are sent again.
* A send's memory is reserved, not assumed: on `main` the SDK's copies of a
  20 MiB send, about 100 MiB, were not counted at all.
* A failed reservation is `limit_exceeded`, and the send can be retried.

### Negative Consequences

* The core may admit 356 MiB instead of 256 MiB; a send of the largest
  attachment uses about 120 MiB at its peak.
* The copy count follows matrix-sdk 0.19.1. An SDK update must recount it
  (`media::SENT_COPIES`).
* The contained worker also uses the default budgets. It never reserves in
  `OutgoingSend`, so its classes still admit at most 200 MiB, within its
  256 MiB Job limit; its aggregate ceiling no longer binds below them.
* Tested with unit tests of the reservations and the class sums; peak
  memory during a real send is not measured.

## Links

* Refines [ADR-0029](0029-send-and-save-files-through-the-cores-own-dialogs.md)
  (files) and the image send of
  [ADR-0009](0009-strip-sent-image-metadata-and-load-received-images-on-request.md).
