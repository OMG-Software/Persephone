# Session owner: lock and unlock while running

Checked 21 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix SDK SQLite stores 0.19.1. P2 "session owner", first slice: the core's
[`Session`](../../crates/core/src/session.rs) is the single authority over the
[protected store](protected-store.md), and the user can lock and unlock it while
the application runs. **Result: locking refuses new work and late results at
once, clears the page, waits for admitted work and only then closes the stores;
a timeout keeps everything owned; no view or room event reaches the page while
locked.** No Matrix SDK client exists yet, so this does not exercise SDK
background tasks.

## Design

Follows the [lifecycle contract](../architecture/sdk-lifecycle.md) and the P0
[ownership fixture](sdk-lifecycle.md), moved into the core:

* **Admission:** all store work holds a `Lease` (at most 8 at once). A lease
  keeps the stores open; its result is published only through
  `Lease::commit`, which refuses once a lock has begun (the generation
  advanced).
* **Lock:** admission closes and the generation advances; the core drops every
  room subscription and pending room event and sends `session_changed` to the
  UI **before** waiting; then it waits for leases to end and closes the four
  stores, and only then releases the profile lock. A wait longer than 5 s
  replies `locking` with the stores still open and owned; locking again
  resumes the wait without announcing again. A failed close gives
  `lock_failed`, which refuses unlock until restart.
* **Drain proof:** a completed lock returns a `TrackedDrain`, whose
  `require_full_sdk_drain()` always refuses: leased work is not an SDK-wide
  drain.
* **Unlock:** reopens through the vault with the same fail-closed rules as at
  start; `key_missing` and the other open failures may retry, `locking` and
  `lock_failed` may not.
* **Views while locked:** `room_list`, `timeline_page` and `subscribe` reply
  `locked`, and demo changes queue no room events. Without `--profile`, the
  core serves synthetic data as before; there is nothing to protect.
* **Exit:** the core locks through the same path after the UI closes (30 s
  wait) and exits 1 if the wait or the close fails.
* **Protocol and UI:** new calls `lock_session` and `unlock_session` (reply: a
  store state), event `session_changed {}`, error `locked`; contract states
  `locked`, `locking` and `lock_failed`. The UI forwards the event on the page's
  one session channel (`watch_session`) and drops every room channel; the page
  clears its rooms and timeline and refetches status. All three commands are in
  the allowlist and capability.

## Fixed along the way

The first session tests aborted the test process (`0xc0000409`). Dropping open
`Stores` without `close()` ran the SQLite pool's destructor, which calls
`tokio::spawn_blocking`, outside a Tokio runtime; the panic recurred during
unwinding and the process aborted. The core's own paths always closed
explicitly, but any panic while unlocked would have aborted the process.
`Stores` now drops the SDK stores inside its runtime, and `close()` drops them
inside the runtime too. A regression test drops open stores, then reopens the
profile and reads back the marker.

### Review fix: lease release order

Review found that `Lease::drop` counted the lease out and woke the locker
before releasing its reference to the stores (Rust drops fields only after
the `drop` body). A locker woken in that window saw no leases, failed to take
sole ownership of the stores and reported a permanent `lock_failed` for a
normal lock. The lease now releases its reference first. If the stores are
ever still shared when a lock tries to close them, the session stays
`locking`, keeps them, and a later lock retries. A new test ends a lease
during a waiting lock 200 times and requires `locked` each time; it did not
reproduce the failure before the fix (the window is a few instructions wide),
so the fix rests on the drop order, not on that test. After the fix,
`cargo test --locked -p persephone-core` passed all 32 core tests and Clippy
passed; the full workspace check below was not rerun locally.

### Review fix: stale replies after a session change

Review also found that the page could show a room list again after locking:
a `room_list` or `app_status` reply to a call made before the lock could
arrive after the page cleared itself, because the host completes commands
concurrently. The page now advances a generation on every session change and
discards replies to calls made under an earlier one (the timeline already
discarded them through the selected room). `python tools/dev.py frontend`
passed, and the real-app walkthrough below was repeated after rebuilding with
the same results; the first repeat lost its DevTools connection before
locking.

### Review fix: room operations across a session change

A further review found that room operations checked only the room ID. After
a lock and unlock, reopening the same room matched again, so an older
`timelinePage` or `subscribeRoom` reply could update the new timeline or set
up a subscription. `open`, `follow`, `refresh` and `older` now require both
the room and the session generation they started under, a subscription that
resolves too late is ended at once, and a session change also clears the
loading flags. `python tools/dev.py frontend` passed and the walkthrough was
repeated after rebuilding.

One repeat (after two attempts that lost their DevTools connection) found the
crypto store's `-wal` and `-shm` files present right after the lock, with
the profile lock released and the state `locked`. Four further runs, checking
right after the lock and again 2 s later, found none. The cause is not
established. Each probe attempt starts by deleting the scratch profile with
errors ignored, so files from an interrupted earlier attempt may have
survived; this was not confirmed, and the final walkthrough below, which also
followed two lost-connection attempts, found none. The WAL holds the same encrypted values as
the database ([P0 storage evidence](storage.md)).

### Review fix: session channel before session content

The page started `watchSession` and its first load at the same time. A session
change before the channel was registered would be missed, because the host
does not replay it, and a failed registration would leave the page showing
content it would never be told to clear. The page now waits for the channel
and loads nothing from the session if registration fails.

## Tests

**Final run, commit `089443e` (all review fixes):**
`python tools/dev.py check --offline` exited 0 with a clean working tree and
passed 205 tests (178 Rust, 12 frontend, 2 server-registration and 13
controller tests), with formatting, Clippy and contract checks. The
walkthrough below was repeated on that build with the same results (no rooms
or items while locked, none 7 s later, profile released and no sidecars right
after the lock or 2 s later, live updates after unlock, exit 0); two earlier
attempts lost their DevTools connection.

**Original run, commit `3017d2e` (before the review fixes):**

`python tools/dev.py check --offline` passed 204 tests (177 Rust, 12 frontend,
2 server-registration and 13 controller tests), with formatting, Clippy and
contract checks. The review fixes added the lease stress test. New at that
commit:

* Session (5, real SDK stores, stand-in key protector):
  * A lease running on another thread keeps a lock waiting: the announcement
    has already run, new admission is refused, another owner gets
    `profile_in_use`, and the lease's later commit is refused. The lock then
    returns `locked` with a drain proof that refuses `require_full_sdk_drain`;
    no WAL/SHM sidecars remain. Unlocking reads back the lease's marker, and a
    new lease commits.
  * A 50 ms timeout returns `locking`, keeps the profile held and refuses
    unlock; after the lease ends, locking again completes without a second
    announcement; locking a locked session changes nothing.
  * Unlocking with the key file removed gives `key_missing` and makes no key;
    restoring it unlocks.
  * The ninth concurrent lease is refused.
  * Without a profile, nothing is admitted and lock/unlock change nothing.
* Store (1): open stores dropped without closing shut down without panicking
  and keep their data.
* Serving loop (1, real pipes): lock replies `locked` and sends one
  `session_changed`; views and subscribing then reply `locked` and a change
  queues no event; unlock replies `protected` with one more `session_changed`;
  views return.
* UI (1, mock runtime): a session change drops room channels and reaches the
  session channel as `null`; no room message follows; `lock_session` relays
  the state and a state outside the contract is refused.
* Protocol: the event's wire form, and extra fields on the new calls and event
  are refused.

## Real application walkthrough

Recorded on the original commit; repeated on the final commit as noted above.

`persephone --profile <scratch dir> --demo-live`, driven over DevTools; the
profile lock was probed with a non-blocking lock on `profile.lock`:

| Step | Page | Profile |
| --- | --- | --- |
| Start, open the planning room | "Local store protected", **Lock** button, 101 items including a live message | Lock held |
| Press **Lock** | "Local store locked", no rooms, no timeline items, "The session is locked. Nothing from it is shown.", **Unlock** button | Lock released, no WAL/SHM sidecars |
| Wait 7 s (the demo keeps posting) | Still no rooms or items | — |
| Press **Unlock** | "Local store protected", 6 rooms, **Lock** button | Lock held |
| Reopen the planning room | Live messages arrive again ("Live demo message 2" → "3") | — |
| Close the window | — | Exit 0, no stderr, no sidecars, lock released |

The lock probe reads "held" before locking and "released" after, so it can
tell the two states apart.

## Limits

* No SDK client: once one runs over these stores, its private tasks are not
  leases, and a completed lock must not be reported as a full drain (the drain
  proof already refuses). See the [lifecycle contract](../architecture/sdk-lifecycle.md).
* Nothing in the application takes a lease yet besides the tests; views still
  come from synthetic fixtures, not the stores.
* Unlock needs no user action: DPAPI unwraps the key for the same Windows user
  (see the [store limits](protected-store.md#limits)). Lock protects against
  use of the open session, not against that user's other processes.
* Logout, account switching and store handoff are not built. A `locking`
  session that never drains stays locked until the application exits; exit then
  reports failure without closing the stores cleanly.
* Debug builds on one Windows host only.
