# SDK lifecycle completion contract

20 October 2026. This defines the remaining integration requirement identified
by the [pinned SDK task inventory and tests](../evidence/sdk-lifecycle.md).
It does not select a new UI framework, SDK fork or extra service process.

## Required ownership

The Rust session owner must close protected admission and invalidate the old
generation at lock/logout start. Clear the UI immediately, then retain every
already-admitted non-cancellable operation until its result is observed. Signal
sync loops to stop between complete operations. Pending work remains owned after
a deadline or caller cancellation; neither event authorizes a replacement,
store release or successful lock status.

Application jobs need bounded task registration, retained joins, callback
publication fences, explicit errors and idempotent retry of a pending stop.
The new fixture tests these pieces, but they do not account for private SDK
workers. A caller-supplied flag must never promote partial coverage to a full
SDK completion proof.

## Current implementation

The core's [`Session`](../../crates/core/src/session.rs) implements the
application side of this ownership for the protected stores: bounded leases,
generation-fenced commits, announcement before waiting, retained ownership
after a lock timeout, and store close only after the wait completes. Its drain
proof refuses to stand for a full SDK drain. See the
[session owner evidence](../evidence/session-owner.md).

Sign-out first attempts to write a local marker for crash recovery. Since
13 November 2026 it then starts the lock: it closes admission, advances the
generation and announces the UI clear, before the homeserver logout request.
The request runs under the lease it was admitted with, and the lock waits for
that lease. After the request, sign-out attempts to record the revocation
result and remove the stored key, then finishes the lock. Before that date the
request ran with admission still open, which did not meet the immediate
logout-start requirement above. [ADR-0039](../adr/0039-obtain-sdk-stop-and-join-upstream-instead-of-forking.md)
records the decision to obtain the SDK completion surface below upstream
rather than fork the SDK or gate on runtime task counts.

The core now runs an SDK client over those stores
([sign-in evidence](../evidence/matrix-sign-in.md)). Its sync loop and sign-in
take a lease per complete operation, but the SDK's own background tasks do not.
Because the completion surface below is still missing, a session whose client
ran locks to `restart_required`: the stores close and are never reopened in that
process.

As of 1 November 2026, client setup also subscribes the SDK event cache before
sync. Local message search reads that cache without HTTP or a separate index.
The subscription activates SDK room-update, ignore-list, cache-shrink,
redecryption, thread-subscriber and persistence work. This work is part of the
remaining SDK task inventory. A completed search or application lease does not
prove that these SDK tasks stopped. The `restart_required` rule remains.
See [room tools](../room-tools.md) and its [verification record](../evidence/room-tools.md).

## Required SDK completion surface

The preferred resolution preserves SDK-owned cryptography and adds or obtains
a supported SDK stop-and-join contract. A reviewed adapter then uses it from the
Rust session owner. The contract must:

- Stop future producers and handler admission for every activated task group.
- Retain and join native response processors and encryption work, even after a
  cancelled outer request or shutdown waiter.
- Join initialization, backup upload/download listeners and their children,
  recovery observers and historic-bundle tasks; account for opted-in cache,
  send-queue, import/export and other blocking work.
- Preserve pending shutdown ownership across timeout/caller cancellation.
  Return failure when an admitted task fails or the inventory is incomplete.
- Avoid cancelling non-cancellable response/store processing midway merely to
  meet a deadline. Expose completion separately from successful initialization,
  successful key delivery or successful remote token revocation.
- Permit database close/store-owner replacement only after relevant writers
  have completed, with any close failure leaving a protected partial failure.

The pinned release has partial wait/stop/pause functions but no operation that
satisfies this complete contract. Its initialization waiter consumes one stored
join handle and returns unit; outgoing encryption requests can log failures.
An adapter must not infer encryption health from completion alone.

## Acceptance before production use

Use real encrypted state/crypto/event/media stores and disposable homeservers.
Hold native/classic response handlers, key upload/download and backup children,
queue sends, pagination and database operations while stop is requested. Prove:

- The old generation cannot publish or admit new protected work.
- Timeouts, cancellation, task panic and close failure cannot authorize a new
  writer or report complete shutdown.
- Every task and child is observed before successful store release; stale work
  cannot write after a new account/store owner starts.
- Repeated stop/lock/unlock and logout followed by another account preserve the
  intended identities, keys and store isolation.
- Remote revocation failures remain visible separately from local shutdown.
- Each supported desktop platform passes the resulting lifecycle and packaging
  checks; key-memory erasure requires its own evidence.

A private runtime or process can be evaluated if supported SDK completion cannot
be obtained, but it needs a separate architecture decision and validation of
persistence under cancellation/termination, blocking tasks and platform behavior.
Tokio `shutdown_timeout`/`shutdown_background` may leave work running and are
not a completion acknowledgement. These alternatives were not implemented here.

[ADR-0008](../adr/0008-restart-the-core-process-to-finish-a-lock-after-the-client-ran.md)
(proposed, 24 October 2026) evaluates the process route. A lock or sign-out
after a client ran would close the stores as today, keep the profile lock,
start a successor core and exit. The successor opens the profile only after
the OS released that lock. This does not satisfy the completion surface above,
and it does not permit a reopen in the same process. The
[upstream status review](../evidence/sdk-lifecycle.md#upstream-status-and-production-configuration-review)
found no supported SDK contract on 24 October 2026.
