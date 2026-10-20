# P0 platform sandbox feasibility

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

**Checked:** 19 October 2026, Windows 11 Pro x64, build 26200, Rust/Cargo 1.99.1.
Network differential rerun 20 October 2026 on the same host; third worker policy
(AppContainer + restricted token) added the same day.
**Status:** Environment admission is fixed and the worker executes its restriction
checks. The complete sandbox gate still exits **1**. Network denial is no longer
inconclusive: an endpoint-observed differential shows that the zero-capability
AppContainer worker **is not network-denied**. Its packets reach the local TCP
stack, and it can map local port state. A third policy — an AppContainer whose
base is a write-restricted, untrusted-integrity restricted token — was added and
**cannot initialize** (`STATUS_DLL_INIT_FAILED`), so no tested policy denies the
kernel socket. CPU accounting, signed packaging, Linux and macOS remain open.

## Current executable evidence

The [test program](../../tests/platform/windows) uses synthetic files,
an ephemeral loopback listener and a temporary AppContainer profile. Before the
restructure, the optimized binary ran with a fresh directory containing spaces
and non-ASCII characters. After the restructure, `python tools/dev.py sandbox
--offline` reran the optimized program in a fresh generated directory and again
exited 1. [Machine-readable results](windows-results.json) now preserve that
rerun's exact output, binary hash and normalized source/manifest/lock hashes.

| Check | Current result |
| --- | --- |
| Environment and storage paths | Redirected `LOCALAPPDATA`, `TEMP`, `TMP` match expected container paths and differ from broker paths |
| Synthetic parent secret variable | Absent; no ambient-environment fallback |
| Worker token | AppContainer membership confirmed |
| Two generated file reads | Both fail specifically with `ERROR_ACCESS_DENIED (5)` |
| Child process creation | Fails with `ERROR_NOT_ENOUGH_QUOTA (1816)` under the one-process Job limit |
| Allocation probe | Successive 4 MiB allocations fail within the tested 16–64 MiB window under a 64 MiB process-memory Job limit |
| TCP loopback exchange | Connect times out without a raw OS error; listener observes no worker connection |
| Host network controls | Same listener completes both host challenge/ACK exchanges; no worker exchange observed |
| Closed loopback and local-interface ports | Worker receives `WSAECONNREFUSED (10061)` in about 2 s, matching host controls: **packets reach the TCP stack** |
| Open IPv4/IPv6 TCP and IPv4 UDP endpoints | Endpoints observe 0 worker connections and datagrams; host controls pass before and after |
| AFD socket device (`\Device\Afd`, native NT path) | Worker opens it with read/write access |
| Network denial verdict | **Not established** (decisive negative): listener-side filtering only, with a port-state oracle |
| LPAC comparison | `WSAStartup` fails with `WSASYSCALLFAILURE (10107)`, but the AFD device still opens; **not a denial pass** |
| Restricted-token comparison | AppContainer + write-restricted, untrusted-integrity token via `CreateProcessAsUserW`: the LowBox worker fails DLL initialization (`STATUS_DLL_INIT_FAILED`, `0xC0000142`) before any probe runs; **no running worker, no socket verdict** |
| Windows capability diagnosis | API succeeds with `missing_capability=NONE` for loopback in both broker and a separate AppContainer process; does not explain the timeout |
| Broker deadline | Two-second deadline terminates a separate spinning worker; termination confirmed |
| OS CPU-time limit | Configured at 30 seconds; **not independently demonstrated** |
| Cleanup | Temporary profile deletion succeeds and generated canary directory is empty |

The allocation probe is not a decoder peak-memory measurement or a total app
memory bound. The broker deadline is not an OS wall-clock limit. The separate
policy-diagnostic process uses the same AppContainer launch path; its API result
is supplementary metadata, not proof of socket permission. The complete gate
still requires network denial and has not been weakened to accept a timeout.

## Network denial differential

A client-side connect timeout cannot separate a dropped packet from a slow or
unreachable listener. Rust's `connect_timeout` also creates the timeout itself,
so no OS error is available. The test program now launches one contained worker
per controlled endpoint (`--net-probe`), using the same AppContainer launch
path. Each endpoint has host positive controls before and after the attempt and
counts what it actually receives.

| Endpoint | Host controls | Endpoint saw worker | Worker result | Verdict |
| --- | --- | --- | --- | --- |
| IPv4 loopback TCP, open | Challenge/ACK before and after | 0 connections | Timeout (3 s) | Dropped |
| IPv4 loopback TCP, closed | Refused, ~2045 ms before and after | — | `10061` refused, ~2000 ms | **Reached** |
| IPv4 loopback UDP | Datagram received before and after | 0 datagrams | Sent locally | Dropped |
| IPv6 loopback TCP, open (optional) | Challenge/ACK before and after | 0 connections | Timeout (3 s) | Dropped |
| Own-interface IPv4 TCP, closed (optional) | Refused, ~2050 ms | — | `10061` refused | **Reached** |

The closed port is the discriminator. With no listener, nothing filters the
inbound SYN, so the TCP stack answers with a reset. A worker that also receives
`WSAECONNREFUSED` sent packets that crossed the container boundary. Open
endpoints were protected by listener-side filtering, not sender-side denial.
The closed-port result also gives the worker a port-state oracle: open ports
time out and closed ports refuse. The own-interface endpoint uses a closed port
only, to avoid opening a non-loopback listener that could invoke firewall UI.

The program also opens the kernel socket device through its native NT path,
`\Device\Afd\…`, with read/write access, and compares the result with the broker.
The AppContainer worker opens it, which matches its demonstrated socket use and
validates the probe. A first version opened `\\.\GLOBALROOT\Device\Afd` and
reported `ERROR_ACCESS_DENIED` even for the AppContainer worker that was creating
sockets. That probe was replaced, and an AppContainer positive control is now
required before any device denial is credited.

A Less Privileged AppContainer (LPAC) comparison opts the worker out of ALL
APPLICATION PACKAGES grants. `WSAStartup` then fails with `WSASYSCALLFAILURE
(10107)`. Rust std asserts on that failure, so the probe calls it first and
reports the code instead of panicking. The AFD device still opens under LPAC,
so direct device I/O beneath Winsock is not excluded, and this is not a denial
pass. LPAC is comparative evidence only: the gate's other checks did not run
under it.

A third policy combines the AppContainer launch with a restricted token. The
broker derives a restricted version of its own primary token with
`CreateRestrictedToken` (`WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE`, the sole
restricting SID `RESTRICTED` S-1-5-12), lowers integrity to Untrusted
(S-1-16-0), and launches the worker with `CreateProcessAsUserW`, keeping the
same `SECURITY_CAPABILITIES` AppContainer attribute and Job limits. Because the
token is a restricted version of the caller's, this needs no assign-primary-token
privilege. `CreateProcessAsUserW` succeeds, but the LowBox worker then exits with
`STATUS_DLL_INIT_FAILED` (`0xC0000142`) during DLL initialization, before it can
run any probe. The identical failure occurs with and without the integrity drop,
so the restricting-SID set alone is decisive: a set tight enough to deny
`\Device\Afd` (RESTRICTED only, on no DACL that grants this worker write) is also
too tight for the loader to open `\KnownDlls` and the process's own station and
heap objects, which are granted to the logon and user SIDs; a set loose enough to
boot would also grant the device. The probe reports
`socket_device[restricted_token]: POLICY_NO_WORKER_RESULT(...0xC0000142...)` and
does not credit any socket verdict from a worker that never ran. Because network
denial was not established under this policy, the full combined gate was not
re-run under it (`restricted_token_full_gate: NOT_RUN`).

Thirteen earlier unit tests plus five new ones (26 in the package) cover the probe
encodings, the endpoint/device classifiers, the factored worker-gate assertion,
and the unexpected-exit (NTSTATUS) decoder. Network denial
passes only if no endpoint observes the worker, every worker attempt is
explicitly denied or silently dropped, required endpoints cover open, closed and
datagram shapes, and a Winsock initialization failure counts only when the
validated device probe denies access. A refusal on a closed port fails.

## Environment failure: cause and correction

The base-only environment originally failed creation with Win32 203. Adding
`LOCALAPPDATA` allowed startup, but the previous launcher replaced that input
with the already redirected `GetAppContainerFolderPath` result. The worker then
rejected all expected path comparisons.

Changing only that input back to the broker's base `LOCALAPPDATA` made every
existing strict comparison pass. This supports double redirection as the cause:
Windows performs the AppContainer redirection itself. The launcher now passes
only `SystemDrive`, `SystemRoot`, `windir` and base `LOCALAPPDATA`. It does not
copy broker `TEMP`/`TMP` or credentials. The worker accepts the OS-added temporary
variables only after verifying all three container paths and confirming they
differ from broker paths. Unknown names and the synthetic parent secret still
cause rejection before protected operations.

Microsoft documents this profile/temporary-directory behavior in
[Launch an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer#creating-the-profile).
The expected path comes from
[`GetAppContainerFolderPath`](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-getappcontainerfolderpath),
not a hardcoded profile path. Comparison preserves UTF-16, folds ASCII case and
rejects relative paths; it is not a general filesystem canonicalizer.

## Additional launcher and evidence fixes

The former quoting helper failed to double trailing backslashes before its
closing quote, potentially merging adjacent arguments. The new
[quoting module](../../tests/platform/windows/src/command_line.rs)
implements Windows escaping without lossy conversion. Paths stay in native
UTF-16 through command construction and worker parsing; embedded NUL is rejected.
Tests use the actual `CommandLineToArgvW` parser with 3,906 generated combinations,
a following sentinel, trailing backslashes, whitespace, Unicode and unpaired
surrogates.

The old result decoder read environment booleans from bits occupied by socket
error codes. The verified marker is emitted only after successful environment
admission; that marker now determines the environment report. Rejection reports
retain their separate flags. Regression tests vary socket payloads, check
rejection reporting and reject non-worker exit codes. Another test prevents
distinct invalid-UTF-16 paths from collapsing to the same replacement character.

## Launch and verification

The worker starts suspended with an AppContainer SID and zero capabilities,
without inherited handles. The broker assigns a Job before resuming it. The Job
caps active processes at one and memory at 64 MiB, and kills remaining processes
on close. Canaries use exclusive creation; cleanup removes only files this run
created. Handles, attribute lists and API-allocated strings have scoped cleanup.
No firewall policy, loopback exemption, persistent capability or real credential
was changed.

```powershell
$env:CARGO_HOME = "$PWD\.local\p0\cargo-home"
$env:CARGO_TARGET_DIR = "$PWD\.local\p0\platform-target"
cargo fmt --manifest-path probes/p0/platform/windows-sandbox-probe/Cargo.toml -- --check
cargo test --offline --locked --manifest-path probes/p0/platform/windows-sandbox-probe/Cargo.toml --all-targets
cargo clippy --offline --locked --manifest-path probes/p0/platform/windows-sandbox-probe/Cargo.toml --all-targets -- -D warnings
cargo build --offline --locked --release --manifest-path probes/p0/platform/windows-sandbox-probe/Cargo.toml
$env:PERSEPHONE_PROBE_DIR = "$PWD\.local\p0\new-synthetic-run"
& .local/p0/platform-target/release/persephone-windows-sandbox-probe.exe
```

Formatting, all **8 tests**, Clippy and the optimized build completed with exit 0.
The native run completed with exit **1**, preserving the failing network gate.
It needed unsandboxed tool access for AppContainer APIs, not administrator
elevation. Cargo occasionally copied incremental-cache files after a hard-link
warning; commands completed successfully.

## Remaining acceptance work

None of the three tested configurations denies networking at the sender. The
AppContainer-plus-restricted-token configuration this section previously proposed
was built and tested: it cannot initialize, because the restriction that would
deny the socket device also prevents the loader from starting the process. The
remaining options are therefore a different socket-denying mechanism that does
not depend on the worker's own token — for example a broker-owned
listener/WFP-based egress control or a kernel-level socket filter — or an
explicit, owner-approved residual-risk decision for listener-side filtering and
the port-state oracle. Non-local internet egress and raw AFD socket requests were
not tested. Exercise CPU accounting, decoder scratch/peak memory and signed
packaged launch. Production handle/IPC behavior remains untested.

No Linux desktop sandbox or macOS helper was built or run. Linux candidates
remain namespaces/Bubblewrap plus seccomp and resource controls; macOS requires
a signed sandboxed helper and reviewed IPC/entitlements. WSL homeserver testing
does not establish desktop Linux worker containment. P0 remains open.

Primary references: [Windows parsing rules](https://learn.microsoft.com/en-us/cpp/c-language/parsing-c-command-line-arguments),
[`CommandLineToArgvW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-commandlinetoargvw),
[Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects),
[`NetworkIsolationDiagnoseConnectFailureAndGetInfo`](https://learn.microsoft.com/en-us/windows/win32/api/networkisolation/nf-networkisolation-networkisolationdiagnoseconnectfailureandgetinfo).
