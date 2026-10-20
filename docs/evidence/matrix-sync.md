# P0 Matrix SDK and synchronization evidence

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

19 October 2026. Investigation against exact Matrix Rust SDK **0.19.1**;
this is not production integration or P0 acceptance.

## Candidate and reproduced behavior

The [isolated crate](../../tests/matrix/Cargo.toml) disables SDK defaults
and explicitly enables `e2e-encryption` and `rustls-aws-lc-rs`. It uses an
in-memory store; persistent-store protection is investigated in the
[storage probe](storage.md). The SDK declares Apache-2.0 and Rust 1.96.
The Windows toolchain used here is Rust/Cargo 1.99.1. All Cargo resolutions are
locked. The HTTP client disables ambient proxies and redirects for these
literal-loopback tests; this does not settle the product's proxy policy.

Nine SDK-backed tests passed, exit 0:

* Native capability selects Sliding Sync. A successful response without that
  compatible capability selects classic sync, including an explicitly false flag.
* HTTP 401/403/404/429/500/503, malformed JSON, wrong feature types, timeout,
  and a TLS handshake failure return errors rather than classic fallback.
  The TLS fixture points HTTPS at a plain HTTP listener; certificate/hostname
  validation and redirect/origin policy need additional tests.
* SDK `available_sliding_sync_versions()` hides a failed `/versions` request,
  reproducing the reason to use fallible `supported_versions()` discovery.
* A classic-only synthetic server is consumed through the real SDK's classic
  sync path, including E2EE support compiled in; its checkpoint is returned and
  no Sliding Sync request is sent. Synthetic key endpoints are not an E2EE
  interoperability proof.
* A paused response event handler still completes after its outer sync task is
  aborted and awaited. This reproduces the narrow cancellation/drain hazard
  rather than relying only on source inspection.

The pinned SDK recognizes the native feature using Ruma's `Msc4186` feature
mapping and the advertised `org.matrix.simplified_msc3575` flag. The fixture
with only `org.matrix.msc4186` and Matrix v1.15 falls back because that label
alone is insufficient for this SDK. Do not replace SDK compatibility discovery
with a homeserver-name check or infer support from an empty convenience result.

## Live disposable Merovingian and Synapse results

Merovingian **0.12.29** and Synapse **1.163.0** ran in WSL with fresh SQLite
databases, synthetic users, loopback client listeners and federation disabled.
Synapse used a workspace-local virtual environment, statistics disabled, no
federation listener, and no trusted remote key servers.
The [server harness](../../tools/servers/run.py) generated all configuration
and credentials independently of production. Server state was removed after
the runner stopped successfully. Its local profile/logs remain ignored under
`.local/p0/live-run-06`; never use these helpers with production credentials.

The live Rust executable exited **0** and verified:

1. Fallible discovery selects native Sliding Sync, then synthetic password login.
2. A synthetic public room is visible through a native Sliding Sync list.
3. Another Sliding Sync request, history pagination, and a key query are
   scheduled concurrently on cloned SDK handles without a global client mutex.
   All complete; history returns seven synthetic state/history events on
   Merovingian and six on Synapse.
4. After all operations finish, the Sliding instance is dropped and classic
   sync completes on the same client. Account ID, device ID, and own device
   public keys are unchanged; the room remains in the SDK projection.

[Machine-readable results](live-results.json). Synapse's version was read from
installed distribution metadata. The public-room smoke test does not send an encrypted chat message (the separate [two-device encrypted probe](encrypted-messaging.md) now covers a narrow classic-sync case),
exercise recipient verification, prove history-gap reconciliation, or cover
persistent-store restart. Nor does concurrent scheduling prove every operation
overlaps internally or establish stress/load safety.

## Merovingian startup diagnosis and resolution

The existing local Linux binary at
`C:\dev\Merovingian\build-wsl\src\merovingian-server` reports **0.12.28**.
Its SHA-256 is
`da65677f738734d6cc9718e0c988efe4649999e7150396f7f241d0a1f185c8ae`.
The checkout HEAD inspected was `61815ce8e173f9681bc4fc028d8916b85911278f`;
the existing binary's source correspondence was not established.

A fresh synthetic configuration used SQLite, a newly generated owner-only
32-byte master key and registration token, disabled federation, and loopback
listeners. The first configuration attempt correctly failed validation because
Merovingian requires an HTTPS advertised public URL and token-protected open
registration. The revised configuration retained those requirements and used
the loopback HTTP side of a reverse-proxy configuration. Startup then exited
**80**: the final hardening self-check reported compiler hardening,
FORTIFY_SOURCE, and capability bounding as unconfirmed and refused service.

Inspection established that this existing build used `buildtype=debug` and
optimization `0`. Merovingian's Meson configuration deliberately omits
`_FORTIFY_SOURCE` at that optimization level and warns that startup will refuse
service. The FORTIFY failure also explains the aggregate compiler-hardening
failure. A separate unprivileged launch could not clear its capability bounding
set; it was not evidence that FORTIFY remained broken.

The [build helper](../../tools/servers/build_merovingian.py) produced a
separate release build under Persephone's ignored `.local/`, with optimization
`3`, hardening enabled and `_FORTIFY_SOURCE=3`. It verified those generated
compiler flags and stable hashes of selected tracked build inputs before and
after compilation. [Build identity](merovingian-build.json) records the
checkout, input digest and binary digest; Merovingian's own test suite was not
built or run by this helper.

The runner then executed as UID 1000 with effective and bounding capabilities
both zero and `NoNewPrivs=1`. WSL root was used only to establish that restricted
context using `setpriv` before switching to the ordinary user. No persistent
capability, source or global security setting was changed. The final startup
log reported compiler hardening, FORTIFY_SOURCE and capability bounding enabled,
and `ready=true blockers=0`. The live SDK check then passed against both servers.
The production instance was not contacted and no hardening refusal was waived.

## Cancellation and lifecycle finding

Pinned source shows that Sliding Sync spawns response processing/cache work
that deliberately survives cancellation of its caller. This protects internal
consistency but means **aborting and awaiting an outer task is not proof of SDK
drain**. A deterministic SDK fixture also pauses an event handler, aborts and
awaits the outer sync task, then releases the handler and observes completion.
This proves that narrow surviving-work case; it does not establish a complete
inventory of background SDK tasks. The successful live switch waited for normal completion; it does not
cover forced cancellation, lock/logout during response handling, or SDK-UI
encryption-sync-service shutdown.

The [cooperative drain probe](sync-lifecycle.md) now retains the real SDK request and
tracks a paused handler across a stop timeout. It refuses replacement until
both finish. Its replacement is an admission-only task, not a real classic sync,
and its tracked leases do not inventory all SDK background work.

Production needs an explicitly tested cooperative stop/drain protocol: stop new
work, retain and await admitted processing, and start neither the next sync mode
nor another store owner until drain is established. A drain timeout is a
protected failure, not permission to declare logout/lock complete and reuse the
store. P0 remains open on this requirement. The low-level `SlidingSync::sync_once`
used by this investigation is documented as public for testing; select the
supported product-facing SDK service APIs separately.

## Reproduction and remaining evidence

```powershell
$env:CARGO_HOME = "$PWD\.local\p0\cargo-home"
$env:CARGO_TARGET_DIR = "$PWD\.local\p0\matrix-target"
# This installed LLVM librarian avoids a native MSVC archive-update error here.
$env:AR_x86_64_pc_windows_msvc = 'C:\Program Files\LLVM\bin\llvm-lib.exe'
cargo test --locked --manifest-path probes/p0/matrix/Cargo.toml
cargo clippy --locked --manifest-path probes/p0/matrix/Cargo.toml --all-targets -- -D warnings
```

Initial native builds failed with MSVC `lib.exe` LNK1114/error 5; an unsandboxed
retry also failed. Selecting the installed `llvm-lib.exe` completed the same
AWS-LC backend build. This is a local toolchain workaround, not a change of TLS
or cryptographic implementation. Do not hardcode that path for other platforms.

Run `servers/run.py --help` under Linux/WSL for disposable server options;
install its pinned test requirement into an isolated virtual environment.
Its output must live under ignored `.local/`. Run the built Rust executable
with the generated profile path and an ignored results path while that runner
is alive. It rejects remote IPs, credentials in URLs, and usernames other than
the synthetic `p0_alice`. The server harness disables ambient proxies/redirects.

Remaining: real classic-only server configuration;
unsupported-endpoint classification after advertised support; disconnect and
expired-position recovery; device/to-device changes during pagination and
switching; in-flight cancellation/drain; encrypted cross-client messaging;
protected persistence/restart; actual SDK-UI service behavior; all target OSes.

Primary pinned sources: [SDK client discovery](https://docs.rs/crate/matrix-sdk/0.19.1/source/src/sliding_sync/client.rs),
[Sliding Sync response ownership](https://docs.rs/crate/matrix-sdk/0.19.1/source/src/sliding_sync/mod.rs),
[classic sync and supported versions](https://docs.rs/crate/matrix-sdk/0.19.1/source/src/client/mod.rs),
[SDK-UI sync service](https://docs.rs/crate/matrix-sdk-ui/0.19.1/source/src/sync_service.rs).