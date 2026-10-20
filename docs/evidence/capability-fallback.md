# Capability-selected sync and classic fallback

Checked 19 October 2026 with Matrix Rust SDK 0.19.1, Windows workers and
disposable Merovingian 0.12.29 / Synapse 1.163.0 in WSL Ubuntu 24.04. No
production instance was used. The [sanitized JSON record](capability-fallback.json)
contains the results, independent request counters, execution binary/source
hashes, failed attempts and verification limits. The current UI choices remain
Tauri and the existing local UI stack; this change adds Rust/test tooling only.

## Results

All **30 controlled cases** passed (15 against each server). A separate
**genuinely classic-only Synapse** run also passed: 31 final live cases total.

| Client-facing condition | Observed behavior on each server |
| --- | --- |
| Native capability advertised | Native selected automatically; mutual SAS and exact encrypted/decrypted sync delivery both ways; 18 native pair rounds, zero classic requests |
| Capability absent | Classic selected automatically; mutual SAS and encrypted delivery/history both ways; 15 classic pair rounds, zero native requests |
| Capability explicitly false | Same successful classic behavior, with zero native requests |
| Discovery HTTP 401, 403, 404, 429 or 503 | Error returned before engine creation; neither sync endpoint requested |
| Malformed discovery JSON, string-valued native flag or timeout | Same refusal; no classic or native request |
| Native HTTP 401, 404 or 503, or injected M_UNKNOWN_POS | Native remains selected; one failed native POST and zero classic requests; another driver cycle is refused without sending another sync request |

Positive cases start with two independent unverified accounts/devices. The
existing negative SAS cancellation control remains untrusted, then a fresh
matching SAS flow verifies the expected peer devices. Messages must arrive as
exact decrypted sync callbacks before history reads can satisfy any assertion.
History also confirms encrypted wire events and exact decrypted event IDs,
senders and bodies. Own/peer public device keys and mutual verification remain
equal before and after sending. Both logouts must succeed.

### A real classic-only deployment

The disposable Synapse configuration sets:

```yaml
experimental_features:
  msc3575_enabled: false
```

Its actual `/versions` response advertises
`org.matrix.simplified_msc3575: false`. The same Rust negotiation path selects
classic without a forced classic-mode argument, completes verification and
bidirectional encrypted messaging, and confirms an authenticated POST to the
SDK's native endpoint returns HTTP 404 with `M_UNRECOGNIZED`.
No gateway is involved in this run. It completes 15 classic pair rounds and
zero native driver rounds; the deliberate native endpoint control is separate.

### Controlled origins for both servers

The inspected Merovingian source has no supported switch to disable native
sync. The test-only loopback gateway therefore removes or sets false the native
and alias flags and blocks native endpoints for the absence controls. All other
client API requests go to the fixed disposable loopback upstream. This tests a
controlled client-facing capability view; it does not claim a server-side
Merovingian disable.

The gateway records only counts, with no HTTP payload/header/URL logging.
Accepted cases require zero forwarding failures. Worker checks and gateway
request counts independently agree on the selected protocol; failed native
cases must make exactly one native request. Request handlers are joined before
final counters are read. The gateway bounds request/response bodies to 2 MiB,
disables proxies/redirects, requests identity encoding, and rejects unexpected
content encoding. Those are test-fixture controls, not product containment.

## Implementation and verification

`SyncPair::preferred` in the Rust Matrix fixture requires successful discovery
for both clients, rejects disagreement, and constructs the selected engines.
It does not reinterpret discovery errors as absence. After selection, failed
cycles mark the driver failed and cannot select a replacement. The reusable
forced-mode regression paths retain their prior interface.

The Rust capability worker owns all login, session, SDK, verification, crypto,
sync, messaging and history operations. Python supplies disposable server
response controls, launches workers and validates payload-free reports. These
programs remain integration fixtures; they are not the production coordinator.

`python tools/dev.py check --offline` exited 0: **73 tests passed** (55 Rust,
3 frontend, 2 server-registration and 13 development-controller tests), plus
formatting, strict Clippy, frontend check/build and generated-contract/TypeScript
checks. The new coverage adds 3 Rust negotiation tests and 8 gateway/controller
tests. Existing nonfatal ts-rs attribute warnings remain.

Restoring even a synthetic SDK session starts background encryption metadata
requests. Tests therefore assert discovery and absence of sync before the
explicit cycle, rather than incorrectly asserting no other network activity.
The failed-cycle check counts only sync requests. This distinction reinforces
why full SDK shutdown needs a separate task inventory and drain test.

The root lockfile is byte-for-byte identical to the pre-change snapshot. No
manifest, feature, dependency version or UI implementation changed; advisory
queries and native containment checks were not rerun. The execution binary and
source hashes were captured before the final runs and rechecked afterward.

## Reproduction

Start two fresh native-capable servers using the
[disposable harness instructions](verified-recipients.md#reproduction), then:

```powershell
$env:CARGO_HOME = (Resolve-Path .local/cache/cargo).Path
cargo build --locked -p persephone-test-matrix --bin verified_recipients
python tools/matrix_capabilities.py .local/runs/your-servers/profile.json .local/runs/your-capability-run
```

For the separate real classic-only case, start the same WSL runner with
`--server synapse --synapse-classic-only` and a new output directory, then:

```powershell
& .local/build/rust/debug/verified_recipients.exe .local/runs/your-classic-server/profile.json .local/runs/your-classic-result.json --discover-classic-only
```

Validate both WSL endpoints from the Windows client before running a lengthy
suite. Finish live worker runs before Cargo commands that may replace the
Windows executable. Profiles, raw logs and generated data belong under ignored
`.local`; public reports contain no passwords, tokens, keys, SAS values or chat
content. The four disposable runners exited 0; all six homeserver listeners
from both attempts were independently confirmed closed from WSL.

## Failed attempts and remaining limits

The initial controlled run passed all 15 Merovingian cases but could not reach
Synapse's chosen port from Windows. That endpoint was healthy inside WSL.
Fresh listeners responded from Windows and the final 30-case suite passed;
the cause of that particular loopback reachability mismatch was not established.
Expected disconnects on late gateway writes initially produced Windows
ConnectionAbortedError tracebacks; these are now handled and handler shutdown
is checked before accepting counters. Initial overstrict test assertions and a
Clippy conversion warning were corrected; the final complete check exited 0.

This establishes initial capability-based selection, not arbitrary runtime
switching. A capable server's later endpoint failure stays an error in this
fixture; unsupported-endpoint reclassification requires its own reviewed
policy and safe lifecycle before automatic replacement. Cache freshness,
position recovery, token refresh, full SDK drain and lock/logout ownership
remain open. Persistence through a fallback transition, crash/corruption
recovery, cross-signing, user verification UI, federation and packaged platform
controls are not established by these in-memory scenarios. Prior
[encrypted restart evidence](encrypted-restart.md) retains its separate scope.
P0 remains open.
