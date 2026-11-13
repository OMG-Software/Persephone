# Obtain SDK stop-and-join upstream instead of forking or gating on task counts

* Status: accepted
* Deciders: project owner (decision in the development session, 13 November 2026)
* Date: 2026-11-13

Technical Story: the [SDK lifecycle completion contract](../architecture/sdk-lifecycle.md),
[matrix-org/matrix-rust-sdk#7302](https://github.com/matrix-org/matrix-rust-sdk/issues/7302)

## Context and Problem Statement

The lifecycle contract requires a supported way to stop the Matrix SDK's
background work and join it before the core releases the protected stores.
matrix-sdk 0.19.1 has none. A review of the pinned sources on 13 November
2026 found about thirty task sources. They cover end-to-end encryption set-up,
recovery state, the room-key bundle receiver, backup upload and download and
their children, the event cache and its persistence writes, the send queue,
OAuth refresh, client caches, crypto child work, cross-process lock renewal
and media clean-up. Most expose no join: some can only be aborted, and some
are detached when their caller is cancelled. `Client::pause` disables queues
and closes stores, but it joins nothing.

So a lock or sign-out after a client ran ends in `restart_required`.
[ADR-0008](0008-restart-the-core-process-to-finish-a-lock-after-the-client-ran.md)
(proposed) covers finishing that with a new core process. How should the
project close the remaining gap in the same process?

## Decision Drivers

* The contract forbids runtime shutdown or task disappearance as proof of
  completion. A cancelled task is not a finished one.
* Keep SDK-owned cryptography and explicit features (ADR-0002).
* Do not claim a drain the core cannot prove: `TrackedDrain::require_full_sdk_drain`
  must keep refusing until a supported surface exists.
* Keep upgrade cost low. The task inventory changes when SDK features change.

## Considered Options

* Obtain a `Client` stop-and-join API upstream, and keep `restart_required`
  until it exists.
* A quiescence gate in the core: after joining the core's own threads, disable
  the send queue, drop the client, and wait until the runtime reports no live
  tasks before closing the stores.
* Patch the SDK in a vendored fork, tracking every spawn site and adding a
  `Client::shutdown()`.

## Decision Outcome

Chosen option: "Obtain a `Client` stop-and-join API upstream", because it is the
only option that meets the contract without the project owning a fork of the
SDK's task inventory, and it adds no unproven completion claim.

Until the API exists:

* A lock or sign-out after a client ran still ends in `restart_required`, and
  ADR-0008 remains the candidate for finishing it without a manual restart.
* The core fixes the part of the contract it owns. Sign-out now closes
  admission, advances the generation and clears the page before it sends the
  homeserver logout request. The request runs under the lease it was admitted
  with, and the lock waits for it (13 November 2026, see
  [`session.rs`](../../crates/core/src/session.rs)).

### Positive Consequences

* No fork to rebase on every SDK release, and no custom change to SDK-owned
  cryptography.
* The drain proof stays honest: nothing reports a full SDK drain.
* The sign-out ordering gap in the contract's "Current implementation" section
  is closed.

### Negative Consequences

* The user still restarts to unlock or sign in again after a client ran,
  unless ADR-0008 is accepted and implemented.
* The project depends on upstream timing. The next SDK upgrade must check
  matrix-org/matrix-rust-sdk#7302 and the release notes for a completion API.

## Pros and Cons of the Options

### Obtain a `Client` stop-and-join API upstream

* Good, because the SDK owns its task inventory and keeps it current.
* Good, because it can meet every acceptance point in the contract.
* Bad, because it does not remove `restart_required` now.

### Quiescence gate in the core

The runtime's live-task count (`RuntimeMetrics::num_alive_tasks`, stable in
the pinned Tokio 1.53.3) would decide when to close the stores; a timeout
would leave the session `Locking`.

* Good, because it cancels nothing and fails safe on a task that never ends.
* Bad, because it cannot say which task is still running, and it does not see
  a task's panic or error, so it cannot return the failures the contract asks
  for.
* Bad, because it does not count blocking threads, and tasks that hold the
  client or HTTP connection pools may never let the count reach zero.
* Bad, because it would need the contract itself to be weakened.

### Patch the SDK in a vendored fork

* Good, because it is the only way to meet the contract now.
* Bad, because about thirty spawn sites in several SDK crates must be found,
  tracked and kept current on every upgrade, and a missed site breaks the
  proof without any visible failure.
* Bad, because a fork of the crate that owns the cryptography raises the
  review burden of ADR-0002.

## Links

* Refines [the SDK lifecycle completion contract](../architecture/sdk-lifecycle.md)
* Related to [ADR-0008](0008-restart-the-core-process-to-finish-a-lock-after-the-client-ran.md)
* Related to [ADR-0002](0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md)
