# Restart the core process to finish a lock after the client ran

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-24

Technical Story: the [SDK lifecycle completion contract](../architecture/sdk-lifecycle.md)
and the [upstream status review](../evidence/sdk-lifecycle.md#upstream-status-and-production-configuration-review)

## Context and Problem Statement

A lock or sign-out must not hand the protected stores to new work until all
earlier work that can write to them has stopped. The core's
[session owner](../../crates/core/src/session.rs) drains its own leases and
joins its own threads. It cannot join the Matrix SDK's background tasks:
matrix-sdk 0.19.1 has no stop-and-join operation, and upstream `main` has none
on 24 October 2026. The same gap is open upstream as
[matrix-org/matrix-rust-sdk#7302](https://github.com/matrix-org/matrix-rust-sdk/issues/7302).

So a lock or sign-out after a client ran ends in `restart_required`. The
stores close and the core never reopens them in that process. The user must
close and start Persephone again by hand to unlock or to sign in again. How
can the core finish these transitions without a claim about SDK-internal work
that it cannot prove?

## Decision Drivers

* Never reopen or hand off the stores while SDK work from the old session can
  still run in the same process (the lifecycle contract).
* Do not claim a full SDK drain. `TrackedDrain::require_full_sdk_drain` must
  continue to refuse.
* Keep SDK-owned cryptography (ADR-0002) and the keyless UI process
  (ADR-0004). Do not fork the SDK or add custom cryptography.
* The core must already recover correctly after a crash or power loss at any
  point. Prefer one recovery path to two.
* The user should not have to restart the app by hand to finish a normal
  lock or sign-out.

## Considered Options

* Keep `restart_required` and ask the user to restart (current behavior).
* Wait for, or contribute, a supported stop-and-join contract upstream, then
  reopen in the same process.
* Patch or fork the SDK locally to retain and join its tasks.
* Run each SDK session in a separate child process and end that process at
  lock.
* Restart the core process automatically after the lock closes the stores.

## Decision Outcome

Proposed option: **restart the core process automatically after the lock
closes the stores**. It is the only option that finishes the transition now,
needs no SDK change, and makes no claim about SDK-internal tasks.

The lock keeps its current steps, then adds a restart:

1. Close admission, advance the generation, clear the UI, and drain every
   admitted lease. This is unchanged.
2. Join the sync, security and sending threads, drop the SDK client, and close
   the stores. When the stores close, their Tokio runtime shuts down. This is
   unchanged.
3. When the result would be `restart_required` (all stores closed and a client
   ran), keep the `profile.lock` file handle open. Do not release it with the
   stores. Today [`Stores::close`](../../crates/core/src/store.rs) releases it.
4. Start a successor core process from the same executable. The successor
   gets no keys, tokens or store handles. It waits, for a bounded time, until
   it can take the exclusive `profile.lock`.
5. The old core closes its UI (on Windows, through the UI Job's
   kill-on-close) and exits. The OS releases the profile lock only when the
   old process's last handle closes. The successor uses this release as its
   signal that the old process has stopped.
6. The successor starts a new UI and opens the profile as at a normal start.
   After a lock, the profile asks for the passphrase (ADR-0006). After a
   sign-out, the next start wipes the profile, as the
   [sign-out flow](../evidence/sign-out.md) already does.

Limits:

* Restart only from the `restart_required` result. `locking` (admitted work
  did not finish) and `lock_failed` (a store did not close) keep their current
  behavior. Ending the process then would cancel work that the core still
  owns, or would hide a store failure.
* If the successor cannot take the profile lock before its deadline, it shows
  `profile_in_use` and does not open the profile. It never waits without
  limit and never opens the profile with the lock held elsewhere.
* No child process may inherit the profile lock handle. This includes the UI
  and the media worker.
* The restart is not proof of a drain. It does not authorize a reopen in the
  same process. The contract's preferred upstream route stays open. A later
  ADR can replace this restart if a supported SDK contract becomes available.

### Positive Consequences

* A lock and a sign-out finish without a manual restart.
* After the old process exits, the OS has reclaimed its threads, handles and
  memory. The core does not need to know which SDK tasks were running.
* Finishing a lock uses the same path as recovery after a crash. Tests for
  one also exercise the other.
* No SDK fork, patch or new dependency.

### Negative Consequences

* This is cancellation, not completion. When the stores' runtime shuts down,
  Tokio drops each remaining SDK task at its next `await` point. This already
  happens today. A multi-step update across stores can stop part way, as it
  can in a crash. The recovery path must be shown to tolerate this (see the
  acceptance items below).
* Runtime shutdown waits for blocking tasks, such as SQLite work. A stuck
  blocking task can stop the lock from finishing. The exit path needs a
  deadline and a visible failure, not an indefinite wait.
* The window closes and opens again. The UI must explain this and must not
  look like a crash.
* The successor handoff is per OS. Today the core starts a UI only on Windows.
  Linux and macOS need their own launch, lock-handoff and containment checks.
* Process exit returns memory to the OS. It is not evidence of key-memory
  erasure, which still needs its own evidence.
* The P0 and P2 gates ask for safe SDK stop before store handoff. If this ADR
  is accepted, the owner must change that gate text so that store handoff
  happens only across a process exit. Until then the gates stay as written.

### Acceptance before relying on it

Use real encrypted stores and disposable homeservers. Prove:

* GIVEN a lock after a client ran WHEN the core restarts THEN the successor
  opens the profile only after the old process exited, and no two cores hold
  the profile at the same time.
* GIVEN an old core that does not exit WHEN the successor's deadline passes
  THEN the successor reports `profile_in_use` and does not open the stores.
* GIVEN the core is ended at controlled points (sync response processing,
  key upload, room key sharing for a send, backup download, recovery setup,
  history-bundle handling) WHEN it starts again THEN the identity and
  cross-signing keys are unchanged, keys already shared stay usable, no
  message is sent twice, and sync resumes without missing events.
* GIVEN a sign-out WHEN the core restarts THEN the profile is wiped and a
  fresh signed-out profile opens.
* GIVEN repeated lock, restart and unlock cycles THEN the identities, keys
  and store isolation stay correct.
* The UI and worker processes do not hold the profile lock handle.

## Pros and Cons of the Options

### Keep `restart_required`

* Good, because it is implemented and safe: nothing reopens in the same
  process.
* Bad, because every lock and sign-out after a client ran needs a manual
  restart.

### Upstream stop-and-join, then reopen in the same process

* Good, because it is the contract's preferred route and keeps one process.
* Good, because upstream maintainers already track the related race (#7302).
* Bad, because no release or `main` commit offers it, and we do not control
  the timeline.
* Bad, because the inventory must be renewed after each SDK upgrade.

### Patch or fork the SDK

* Good, because it could deliver a real join sooner.
* Bad, because it changes SDK-owned code next to the cryptography, which
  ADR-0002 avoids, and it adds a fork to maintain and review.

### Separate SDK session process

* Good, because ending the session process is a full stop, as in the chosen
  option, without closing the UI.
* Bad, because keys and stores would move to a third process with its own
  IPC, containment and supervision. That is a larger change to ADR-0004.

### Restart the core process

* Good, because it finishes the transition now, with no SDK change.
* Good, because it reuses the crash-recovery path and the existing exclusive
  profile lock.
* Bad, because it depends on recovery after cancellation, which needs the
  acceptance tests above.
* Bad, because the window restarts, and each OS needs its own handoff checks.

## Links

* Refines [SDK lifecycle completion contract](../architecture/sdk-lifecycle.md)
* Relates to [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
  (core as entry process) and [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md)
  (unlock after restart)
* Evidence: [SDK task ownership and upstream status](../evidence/sdk-lifecycle.md),
  [sign-out](../evidence/sign-out.md), [session owner](../evidence/session-owner.md)
