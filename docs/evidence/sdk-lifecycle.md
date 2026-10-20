# SDK task ownership and cooperative stop

Checked 20 October 2026 on Windows with Matrix Rust SDK 0.19.1 and Tokio
1.53.3. The [machine-readable record](sdk-lifecycle.json) includes the pinned
source inventory, source hashes, test names, results and remaining requirements.
Tauri, the current UI stack, dependency versions and features are unchanged.

**Result: tracked work can be drained cooperatively; the complete SDK shutdown
acceptance gate remains open.** No production lock, logout or store handoff is
implemented or authorized by this prototype.

## Implemented behavior

The new Rust `lifecycle_fixture` owner retains join handles rather than moving
them into disposable stop futures. A stop request closes admission, invalidates
the generation and signals cooperative loops. Loops check that signal between
completed SDK calls; they do not select cancellation against a non-cancellable
response processor. A timeout or cancellation of the stop waiter keeps the
underlying job owned, and a later stop call can observe its completion.

Callback leases perform short synchronous updates under the generation fence.
An update that completes after stop is refused. Errors, panics, poisoned state
or invalid accounting close admission and cannot produce successful tracked
drain. Shutdown observes all registered job outcomes before reporting tracked
success. Both concurrent work and retained handles are capped at eight; this is
a bounded fixture batch, not a production request scheduler. A long-running loop
belongs inside one owned job rather than accumulating a handle per request.

`TrackedDrain` has a private constructor and its `require_full_sdk_drain()`
method always rejects. Completing application-registered work cannot manufacture
proof about SDK-internal work. Dropping the owner closes admission but can detach
unfinished jobs; Drop never reports a drain or releases a protected store owner.
The generation fence protects updates made through this API, not arbitrary
SDK-internal writes.

## Real SDK barrier tests

The controlled tests use real SDK calls with synthetic accounts, tokens and
plain timeline events. Native sync has E2EE extensions enabled; no real-homeserver
Megolm or SAS scenario was rerun in this step.

1. Classic sync reaches a paused SDK callback. Stop times out, new admission is
   closed and the callback's late update is refused. Releasing the callback lets
   the retained frame finish and the loop exit before a second sync request.
2. Native sync follows the same sequence, retaining the outer frame that awaits
   its SDK response processor and scoped encryption wrapper. Exactly one native
   sync request is observed; the owner does not abort that in-flight frame.
3. SDK encryption initialization is held at an actual backup-version HTTP
   request. Cancelling the owner's stop waiter retains the SDK initialization
   waiter. A second SDK initialization wait returns immediately while the first
   remains pending, because the first has taken the SDK's stored join handle.
   `Client::pause()` also returns while that request is still pending. A later
   owner stop waits until the test releases the HTTP barrier.

The pause counterexample uses **memory stores only**. It proves that pause is
not a general task-drain barrier; it does not establish encrypted database lock
release or persistent-store handoff. The initialization barrier returns a
controlled HTTP 503 when released. Joining that task proves completion, not
healthy encryption initialization: the public SDK waiter returns unit and logs
failures internally.

## Pinned task inventory

The JSON record identifies source files, exact matching lines and registry
source hashes. Relevant ownership and activation are:

| Work | Activation and completion contract |
| --- | --- |
| Classic frame and callbacks | A normal call awaits response processing, persistence and inline handlers. Cancellation is not normal completion. |
| Native response processor | Deliberately spawned to survive caller cancellation; its join handle is held by the outer future. Retaining that outer future permits a normal join. |
| Native outgoing encryption wrapper | Joined on successful HTTP completion; aborted on drop. Per-request encryption errors can be logged, so completion alone is not delivery success. |
| Encryption initialization | Login/restore spawns `setup_e2ee`. The public waiter takes that one handle; a cancelled waiter may detach it, and repeated waiting is not equivalent to retaining it. |
| Backup upload listener | Created even with default encryption settings; Drop aborts without a join acknowledgement. |
| Backup downloads and children | Enabled by `AfterDecryptionFailure`, rather than default `Manual`; children can retain a strong Client, and dropping the listener does not recursively join them. |
| Recovery observer | Separately spawned during recovery setup; not covered by the initialization waiter. |
| Historic key bundles | Builder enables history sharing on invite by default. Startup/listener handles lack a release-build public join. The controlled initialization test disables this option explicitly. |
| Event cache | Client constructs the object, but subscriber workers require explicit `subscribe`; existing Matrix fixtures do not activate them. |
| Send queues | Globally enabled, with lazy per-room workers; existing fixtures send directly. Disabling queues is not an in-flight send join. |
| Pause | Disables queues and closes stores after application sync stop; does not join all preceding work. |
| Task monitor | Failure/abort/status observability is not a recursive all-task completion acknowledgement. |

Some tasks are inactive in the current minimal profile; adding SDK features,
queue/cache use or recovery policies requires renewing the inventory. The SDK
has no public Client/Encryption operation that cooperatively stops and joins
all these task groups.

## Required SDK contract

The [implementation contract](../architecture/sdk-lifecycle.md) specifies the
remaining work. The preferred route is SDK-supported retained stop-and-join
ownership, followed by a reviewed adapter and complete lifecycle tests. The
current dependency pin has not been patched, forked or upgraded in this step.

A full completion acknowledgement must cover response processors, encryption
initialization, backup listeners and children, recovery/bundle work, activated
cache/queue workers and admitted application operations. Stop must close future
producers, retain pending ownership across deadlines/caller cancellation and
propagate failures. Protected store ownership and lock success cannot follow
an unknown or timed-out drain. Remote logout/revocation remains a separate
network outcome.

A private runtime/process boundary is an alternative architecture to evaluate,
not a proven shortcut: cancelling SDK writes may leave partial protocol state,
and Tokio runtime timeout can leave blocking work alive. No such boundary or
new UI choice was adopted here.

## Verification and limits

`python tools/dev.py check --offline` completed with exit 0: **85 tests passed**
(67 Rust, 3 frontend, 2 server-registration and 13 controller tests), plus
formatting, strict Clippy, frontend build/check and generated-contract/TypeScript
checks. Twelve new Rust tests cover the owner contracts and controlled SDK
barriers. Expected panic controls are caught and checked for protected failure;
they are not process crashes counted as successes. The test HTTP helper joins
its listener/connection tasks and surfaces unexpected task panics.

An intermediate check failed Clippy after a mechanical edit added a stop handle
without updating the two SDK loops. Both loops now consult it only after their
SDK frame completes; the final complete check passed. Earlier focused and
aggregate runs are retained under ignored `.local/runs/sdk-lifecycle-01`.
Existing nonfatal ts-rs attribute warnings remain.

No production or disposable Merovingian/Synapse instances were started, no
external messages were sent, and no runtime/store lifecycle of real accounts was
changed. The preceding real-homeserver compatibility/restart evidence remains
separate. Persistent store handoff, complete SDK shutdown, queued sends,
backup-child drain, remote logout failure, key erasure, crash recovery and native
Linux/macOS packaging remain unverified. **P0 remains open.**
