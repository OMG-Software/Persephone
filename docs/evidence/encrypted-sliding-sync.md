# Encrypted native Sliding Sync and classic transition

**Checked:** 19 October 2026. **Result:** passed on disposable Merovingian
0.12.29 and Synapse 1.163.0, using Matrix Rust SDK 0.19.1 with the existing
explicit encryption/TLS feature selection. All 418 registry package versions,
sources and checksums match the archived dependency inputs, and the critical
SDK/crypto/HTTP/TLS feature union is unchanged. The current lockfile hash differs
from the earlier ledger; the exact cause of that text change was not established.
The new evidence records the current hash and preserves the previous one.
The Windows test executable and WSL homeservers ran outside the tool sandbox.
No production server or account was used.

## What passed

The [verified-recipient program](../../tests/matrix/src/bin/verified_recipients.rs)
now accepts `--sliding-sync`. It retains the classic-only mode when that flag is
absent. [Machine-readable evidence](encrypted-sliding-sync.json) records the
results, commands, exact binary hash and normalized source/lock hashes.

| Scenario | Merovingian | Synapse |
| --- | --- | --- |
| Native Sliding Sync, then completed-operation classic transition | PASS | PASS |
| Classic-only regression using the updated program | PASS | PASS |

Each server uses two independent accounts and fresh SDK clients with in-memory
stores. In native mode, compatible capability discovery is required and the
first sync is native; an error or absent capability fails the test instead of
selecting classic sync. A small selective room list and explicit subscription
request encryption, membership and supporting room state. E2EE and to-device
extensions are enabled; key upload/query and message sends use the normal SDK
Matrix APIs.

The native stage repeats the earlier SAS checks: initially unverified devices,
a delivered cancellation that leaves both unverified, a fresh flow bound to the
expected user/device on each side, matching authentication strings compared by
the trusted fixture controller, both confirmations and done states, and mutual
SDK device verification. No local-trust setter is called.

Alice sends to Bob and Bob sends to Alice. Before any history request, each
recipient must receive an SDK `OriginalSyncRoomMessageEvent` callback with
decryption metadata and the exact sent event ID, sender, room ID and body.
Missing, plaintext-only or mismatched callbacks cannot satisfy this receipt.
Only then does the test check raw history for that exact encrypted Megolm event
and SDK-decrypted history for the matching message. History retrieval cannot
substitute for native sync delivery.

Both servers completed **18 native pair rounds**, with **zero classic driver
rounds before transition**. These count the fixture's sequential completed calls
for both clients; they are not a packet capture or counts of every SDK request.

## Completed-operation transition

After all native sync calls, SAS operations, message sends, callbacks and history
checks complete, the fixture drops its two native engine handles and starts
classic sync on the same clients. Snapshots before transition, immediately
after classic sync and after new classic messages compare both users' IDs,
device IDs, public device keys, peer keys and peer verification flags. Key
values remain private test data and are not published.

Both earlier native messages must still decrypt from history. Both users then
send new encrypted messages with new event IDs and distinct bodies; their
recipients must receive matching decrypted sync callbacks before history is
checked. Each server completed **3 classic pair rounds** in this stage. Both
authenticated logout requests succeeded.

The [sync driver](../../tests/matrix/src/sync_fixture.rs) refuses a transition
without completed native calls, during an unfinished call, or after a failed
cycle. Caller cancellation leaves the local in-flight flag set, so it cannot be
misrepresented as a completed transition. This is a test-driver guard, not proof
that all SDK tasks have drained after cancellation.

The [receipt recorder](../../tests/matrix/src/sync_receipts.rs) unregisters its
handler on drop, deduplicates room/event IDs and fails closed on oversized
fields, capacity overflow or a poisoned lock. It retains at most 32 records,
with 512-byte identifier and 256-byte body limits. These are fixture bounds,
not bounds on allocations earlier in the SDK/network pipeline.

## Reproduction and checks

Start fresh disposable homeservers using the
[runner instructions](verified-recipients.md#reproduction), wait for the profile,
and use a new result filename:

```powershell
$env:CARGO_HOME = (Resolve-Path .local/cache/cargo).Path
cargo build --offline --locked -p persephone-test-matrix --bin verified_recipients
& .local/build/rust/debug/verified_recipients.exe .local/runs/verified-recipients/profile.json .local/runs/verified-recipients/native-results.json --sliding-sync
```

The recorded native run used `.local/runs/encrypted-sliding-01`. Its separate
classic-only regression passed with the flag omitted; each scenario logged out
both devices. The runner then exited 0 and both loopback listeners were
independently confirmed closed from WSL.

`cargo fmt --all -- --check`, the 19 Matrix-package tests, strict Clippy and
`python tools/dev.py check --offline` all completed with exit 0. The full check
passed **53 tests: 48 Rust, 3 frontend and 2 server-registration tests**, plus
frontend check/build and generated-contract/TypeScript checks. Seven new tests
cover local transition refusal and exact/bounded decrypted receipts. The
existing nonfatal `ts-rs` warning about `deny_unknown_fields` remains; Rust
contract tests separately enforce deserialization behavior.

## Limits and remaining work

This establishes compatibility of the pinned low-level SDK path on these
disposable server versions. It does not establish production coordinator or
high-level sync-service behavior. The deliberate switch on a capable server is
not an automatic unsupported-capability fallback test. Classic-only capability
negotiation and error/recovery behavior remain open.

No in-flight mode switch or forced SDK cancellation was used. The pinned SDK
awaits its scoped outgoing-encryption task and response processing on successful
`sync_once` calls; unrelated tasks and lock/logout drain remain an open gate.
The caller timeouts bound test waits and do not prove complete SDK shutdown.

The controller simulates a trusted out-of-band SAS comparison. Human UI,
cross-signed user identity, restart/recovery, persistent room-key continuity,
device replacement, federation and production key-sharing policy remain
untested. The 8 MiB debug driver stack from the prior test is retained and is
not a product memory measurement. Windows media containment and packaged
Linux/macOS checks were not rerun or resolved. P0 remains open.
