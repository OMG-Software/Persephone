# Development

## Repository layout

The repository groups files by their role. The root Cargo workspace and lockfile cover the shared crates and Rust integration/security test programs. The application starts as `persephone` (the core, `crates/core`), which launches the keyless UI `persephone-ui` (`src-tauri/`) and serves its requests (ADR-0004). Run the app with `.local/build/rust/debug/persephone.exe` after a build; `persephone-ui` refuses to start on its own. Add `--profile <dir>` to open (or create) an encrypted local profile there; without it nothing is stored. Use a disposable directory under `.local/` for development profiles.

```text
Cargo.toml                         # Rust workspace
Cargo.lock                         # One committed Rust dependency resolution
.cargo/config.toml                 # Cargo output under .local/build/rust
src-tauri/                         # Keyless Tauri UI (persephone-ui), capabilities, window policy
crates/
  core/                            # Key-holding core; entry binary `persephone`
  core-protocol/                   # UI-to-core messages (bounded frames)
  contracts/                       # Shared DTO and contract-generation code
  media-broker/                    # Launches the contained media worker (Windows)
  media-decoder/                   # PNG/JPEG decoder built as a wasm32 guest
  media-protocol/                  # Shared byte-budget and framed protocol code
  media-sandbox/                   # wasmi host: import allowlist and resource limits
  media-worker/                    # Contained worker binary; embeds the decoder guest
tests/
  matrix/                          # Matrix sync and encrypted-message integration tests
  storage/                         # Encrypted-store and credential API tests
  platform/windows/                # Windows containment test program
frontend/                          # Svelte static UI feasibility implementation
tools/
  dev.py                           # Repository check and run entry point
  matrix_restart.py                # Disposable Rust worker restart controller
  matrix_capabilities.py           # Controlled sync-capability integration runner
  audit_dependencies.py            # OSV Rust dependency audit
  servers/                         # Disposable Merovingian/Synapse harness
docs/
  adr/                             # Accepted and proposed decisions
  architecture/                    # Architecture and security plans
  evidence/                        # Run records and verification artifacts
  planning/                        # Project plan and feasibility status
  research/                         # Framework security research
.local/
  cache/cargo/                     # Cargo download cache
  cache/npm/                       # npm cache
  build/rust/                      # Cargo build output
  build/frontend/                 # Frontend build output
  runs/                            # New synthetic test profiles and output
  servers/merovingian/             # Local Merovingian helper binary
  servers/synapse/                 # Isolated Synapse virtual environment
```

The `contracts` and `media-protocol` crates are shared building blocks. They remain provisional and are not security-approved application components. The integration and platform programs exercise bounded scenarios; their results do not establish production protections or close the P0 phase. The frontend builds static assets that the desktop shell embeds.

Historical evidence reports preserve command paths, source hashes, and package-resolution facts from before this layout was created. Those old paths identify the recorded run inputs. The [evidence index](README.md#evidence) links the reports; use this guide for current locations and commands.

## Toolchain and commands

Install Python, Rust/Cargo, and Node.js with npm available on `PATH`, plus the WebAssembly target for the decoder guest (`rustup target add wasm32-unknown-unknown`). On Windows, use the MSVC Rust target and its native build tools. Install the locked frontend dependencies once, then run checks from the repository root:

```powershell
python tools/dev.py bootstrap
python tools/dev.py check
python tools/dev.py rust
python tools/dev.py frontend
python tools/dev.py contracts
python tools/dev.py audit
python tools/dev.py sandbox --offline
```

`check` runs Rust formatting, workspace tests and Clippy, frontend check/build/tests, generated-contract drift/TypeScript checks, disposable-server registration tests, and restart-controller tests. It does not run live homeservers, the native containment program or dependency audit. `audit` queries OSV and needs network access; it cannot run offline. Add `--offline` to supported Cargo, frontend, contract, bootstrap, or sandbox commands when using cached dependencies.

The Windows containment run is deliberately separate. On Windows it currently exits 1 because the zero-capability AppContainer worker is not network-denied: an endpoint-observed differential shows its packets reach the local TCP stack. The run also prints a comparative LPAC result, which does not change the gate. On other operating systems the command exits 2 because this program requires Windows; equivalent Linux and macOS coverage remains open. The unresolved gate is recorded in the [feasibility workbench](planning/feasibility.md) and [Windows sandbox evidence](evidence/windows-sandbox.md).

For focused Rust work, use `cargo test --locked -p <package>` from the root. Current package names are `persephone-contracts`, `persephone-media-protocol`, `persephone-media-decoder`, `persephone-media-sandbox`, `persephone-media-worker`, `persephone-media-broker`, `persephone-core`, `persephone-core-protocol`, `persephone-desktop`, `persephone-test-matrix`, `persephone-test-storage`, and `persephone-test-windows`. All resolve through the root lockfile and use `.local/build/rust` for output.

Run the controlled lifecycle tests with `cargo test --locked -p persephone-test-matrix --test lifecycle_owner --test lifecycle_sdk`. They use synthetic loopback HTTP and memory stores. The Rust prototype tracks registered work; it cannot certify full SDK shutdown or production lock/logout.

If the Windows MSVC `lib.exe` AWS-LC archive step fails, set `AR_x86_64_pc_windows_msvc` to an installed LLVM librarian in the current shell and retry. For example:

```powershell
$env:AR_x86_64_pc_windows_msvc = (Get-Command llvm-lib.exe).Source
```

This is a local toolchain workaround. Do not add a machine-specific LLVM path to repository configuration.

### Continuous integration

[`.github/workflows/check.yml`](../.github/workflows/check.yml) runs `python tools/dev.py bootstrap` and `python tools/dev.py check` (`python3` in the distribution containers) for every pull request, every push to `main` and on demand. Every job pins Rust 1.99.1 (with `rustfmt`, `clippy` and the `wasm32-unknown-unknown` target) and Node.js 24.15.0, uses the committed lockfiles, and caches only downloaded Cargo and npm sources. Like the local check, it does not run live homeservers, the native sandbox probe or the dependency audit.

| Job | Runner or image | Prerequisites | Python |
| --- | --- | --- | --- |
| `windows` | `windows-latest` | Preinstalled MSVC | 3.14 (setup-python) |
| `macos-latest` | `macos-latest` (arm64) | Preinstalled Xcode tools | 3.14 (setup-python) |
| `ubuntu-latest` | `ubuntu-latest` (24.04) | `apt-get`: `libwebkit2gtk-4.1-dev build-essential pkg-config` | 3.14 (setup-python) |
| `ubuntu-24.04-arm` | `ubuntu-24.04-arm` (arm64) | As `ubuntu-latest`; rustup from `sh.rustup.rs` if the image lacks it | 3.14 (setup-python) |
| `debian-13` | `debian:trixie` container | `apt-get`: build tools, `libwebkit2gtk-4.1-dev` | Distribution `python3` |
| `arch` | `archlinux:base-devel` container | `pacman -Syu`: `webkit2gtk-4.1` | Distribution `python` |
| `fedora-44` | `fedora:44` container | `dnf`: build tools, `pkgconfig(webkit2gtk-4.1)` | Distribution `python3` |
| `opensuse-leap-16.0` | `opensuse/leap:16.0` container | `zypper`: build tools, `pkgconfig(webkit2gtk-4.1)` | Distribution `python3` |

- **Containers.** Container images are pinned by digest in the workflow. Containers install Rust with rustup from `sh.rustup.rs` and Node.js with setup-node.
- **Packages are not pinned.** Packages come from each distribution's current repositories, so only the base image is fixed. Arch supports only full upgrades, so its job always builds against the current rolling release.
- **Off Windows, only build and tests are covered.** There is no vault, credential prompt, UI containment or worker containment there. These fail closed (`unsupported`), and the core refuses to start the UI. A passing non-Windows job therefore shows that the code builds and its tests pass, not that the application runs or is contained on that OS. No job launches the application, on Windows either.
- **Tests that are not compiled off Windows.** The AppContainer probe, DPAPI, the native-prompt and the contained-worker tests compile only on Windows. This is why Windows runs 41 more Rust tests.

Run 39785251911 at `04c0538` passed every job:

| Jobs | Rust tests (workspace + development feature) | Frontend | Python |
| --- | --- | --- | --- |
| `windows` | 235 (202 + 33) | 16 | 15 (2 + 13) |
| `macos-latest`, `ubuntu-latest`, `debian-13`, `arch`, `fedora-44`, `opensuse-leap-16.0` | 194 (163 + 31) each | 16 each | 15 (2 + 13) each |

On every job, `cargo fmt`, Clippy with `-D warnings` (default and development feature), the contract drift check and the TypeScript check also passed. Each job took 8 to 17 minutes. The `#[cfg(unix)]` directory sync in `crates/core/src/store.rs` compiles on Linux and macOS, and the sign-out and store tests that use it pass there.

### Release builds

[`.github/workflows/release.yml`](../.github/workflows/release.yml) builds,
signs and checksums the installers. A `v*` tag creates a draft pre-release; a
manual run builds unsigned artifacts only. [Release guide](release.md) lists
the secrets, how to cut a release and how users verify the files.

## Disposable homeservers

The harness in [`tools/servers`](../tools/servers) runs only disposable local Merovingian and Synapse instances. Use Linux/WSL and Python 3.12. The current path-bound prerequisites are `.local/servers/merovingian` and `.local/servers/synapse`; fresh synthetic profiles and logs belong under `.local/runs`. The harness rejects production configuration, databases, accounts, and URLs.

Merovingian must run as a non-root UID with empty effective and bounding capability sets and `no_new_privs`. Synapse alone does not need that setup. The recorded live smoke passed for both servers, but it only establishes those harness scenarios. A hard kill can leave synthetic state; cleanup is not secure erasure. The loopback HTTP profile does not approve HTTP or trust bypasses in the product.

### Signing the app in to a disposable server

Default builds accept only `https://` homeservers and ask for credentials in the native Windows prompt. To use the disposable servers, build the core with the development feature, which also accepts `http://127.0.0.1:<port>` and `http://[::1]:<port>`:

```powershell
cargo build --locked -p persephone-desktop
cargo build --locked -p persephone-core --features dev-disposable-servers --bin persephone
```

Run `.local/build/rust/debug/persephone.exe --profile <new directory under .local>` and enter the server's `base_url` from the harness profile in the page. Windows then asks for the username and password (`username` and `password` in the profile). To skip the prompt for automated runs, set `PERSEPHONE_DEV_PROFILE` to the harness `profile.json`; the core then prints `DEVELOPMENT BUILD` and answers the prompt from that file for the matching host. Never ship or distribute a build with this feature. See the [sign-in evidence](evidence/matrix-sign-in.md).

With `--profile`, the core asks for the profile passphrase in its own Windows window before the page opens. A new profile asks for a new passphrase twice (at least 10 characters), and a profile from before passphrases is upgraded: it also asks for a new passphrase twice (ADR-0006). "Lock" closes the profile; "Unlock" asks for the passphrase again, but after a sign-in only a restart unlocks. "Reset profile…" is shown while the profile is closed; the core confirms in its own window, deletes the profile and asks for a new passphrase. With `PERSEPHONE_DEV_PROFILE`, `PERSEPHONE_DEV_PASSPHRASE_FILE` (a path inside `.local`) answers the passphrase prompts from that file; without it the native window is used. See the [unlock secret evidence](evidence/unlock-secret.md).

"Sign out" in Settings, Session, attempts to revoke the session on the homeserver and deletes the profile's key; the next start deletes the store files. Restart Persephone to sign in again. See the [sign-out evidence](evidence/sign-out.md).

Settings, Security & encryption, sets up cross-signing and recovery for an account without them, or verifies this device with the recovery key; the core asks for the password or key in its own window and shows a new recovery key in its own window. With `PERSEPHONE_DEV_PROFILE`, the development build answers the password from the profile, and `PERSEPHONE_DEV_RECOVERY_FILE` (a path inside `.local`) replaces the recovery-key windows with that file. See the [encryption setup evidence](evidence/encryption-setup.md).

The composer sends text in the open room. The live sending check uses a second SDK client: build it with `cargo build --locked -p persephone-test-matrix --bin p4_peer`, then drive it with one command per line (`login bob`, `bootstrap`, `create encrypted|plain|public <user> <name>`, `send`, `expect`, `quit`) against a harness profile. See the [sending evidence](evidence/encrypted-sending.md).

"Verify with another device" in Settings, Security & encryption, compares emoji with another device of the same account on its own verification screen; that screen also opens to accept a request from one. For the live check, the same peer signs in as the first account (`login alice`), restores its keys from the development recovery-key file (`recover <file>`), waits until it knows the new Persephone device (`wait-new-device <seconds>`), and then accepts (`verify-accept <seconds>`) or asks (`verify-request <device id> <seconds>`), followed by `verify-confirm`, `verify-mismatch` or `verify-wait <seconds>`. See the [own-device verification evidence](evidence/own-device-verification.md).

In a direct-message room, the header names the contact and how far the account trusts them; on a verified device, "Verify" asks the contact to compare emoji, and a request from them opens the same verification screen. Requests go only to an existing direct-message room the contact is still in. For the live check, the peer as Bob sets up cross-signing (`login bob`, `bootstrap`) and creates a direct-message room (`create dm <user>`); the first account's own peer device joins and marks it (`login alice`, `alice-join-dm <room>`), because Persephone cannot accept invitations yet. Bob waits until he knows the account's devices (`wait-devices bob <user> <count> <seconds>`), then accepts (`contact-accept <seconds>`) or asks (`contact-request <user> <seconds>`), followed by `verify-confirm`, `verify-mismatch` or `verify-wait <seconds>`; `trust <bob|alice> <user>` reports whether a client sees a user as verified. See the [contact verification evidence](evidence/contact-verification.md).

While a member of an encrypted room has a changed identity, the room shows that sending is paused, and the core refuses every send there (ADR-0007). "Accept new identity…" asks in the core's own window; with `PERSEPHONE_DEV_PROFILE`, `PERSEPHONE_DEV_IDENTITY_ANSWER_FILE` (a path inside `.local`, read each time) answers it: `yes` accepts, anything else declines. For the live check, the peer replaces Bob's identity with `reset-identity [<user>]`, naming a user Bob had verified so that his client withdraws that verification. See the [identity changes evidence](evidence/identity-changes.md).

The remaining P4 runs also use `count <bob|bob2> <room> <seconds> <text>`, which counts the decrypted messages with that text and lists the ones that could not be decrypted, and `sign-devices`, which has Bob sign his own unsigned devices. See the [encrypted text exit runs](evidence/encrypted-text-exit-runs.md).

## Recorded verification

After pausing sends on identity changes (ADR-0007), `python tools/dev.py check --offline` passed all 378 tests (324 Rust, 39 frontend, 2 server-registration and 13 development-controller tests). See the [identity changes evidence](evidence/identity-changes.md).

After adding contact verification, `python tools/dev.py check --offline` passed all 372 tests (319 Rust, 38 frontend, 2 server-registration and 13 development-controller tests). See the [contact verification evidence](evidence/contact-verification.md).

After adding the profile passphrase (ADR-0006), `python tools/dev.py check --offline` passed all 342 tests (306 Rust, 21 frontend, 2 server-registration and 13 development-controller tests). See the [unlock secret evidence](evidence/unlock-secret.md).

After adding own-device verification, `python tools/dev.py check --offline` passed all 298 tests (263 Rust, 20 frontend, 2 server-registration and 13 development-controller tests). See the [own-device verification evidence](evidence/own-device-verification.md).

After the sending review fixes (outbox byte budget, room-ID parsing, unknown encryption refused, queueing under the session lock, ordered pending replies), `python tools/dev.py check --offline` passed all 299 tests (265 Rust, 19 frontend, 2 server-registration and 13 development-controller tests).

After adding encrypted sending, `python tools/dev.py check --offline` passed all 293 tests (259 Rust, 19 frontend, 2 server-registration and 13 development-controller tests). See the [sending evidence](evidence/encrypted-sending.md).

After the fourth round of encryption setup review fixes, the parts of `python tools/dev.py check --offline` passed in separate runs: 250 Rust tests, formatting and Clippy in the full run, whose frontend tests froze after a file-lock error (`EPERM`) in the worktree's Vite cache; then `frontend` (17 tests), `contracts` and both Python test directories (2 and 13 tests) each passed on their own.

After the third round of encryption setup review fixes, `python tools/dev.py check --offline` passed all 281 tests (249 Rust, 17 frontend, 2 server-registration and 13 development-controller tests). The new account test failed on the code before the fix.

After the second round of encryption setup review fixes, `python tools/dev.py check --offline` passed all 280 tests (248 Rust, 17 frontend, 2 server-registration and 13 development-controller tests); the first round passed 278 (246 Rust, 17 frontend, 2 server-registration and 13 development-controller tests); the first version passed 276 (244 Rust, 17 frontend, 2 server-registration and 13 development-controller tests). See the [encryption setup evidence](evidence/encryption-setup.md).

After the portability fixes (commit `e005c7c`), `python tools/dev.py check --offline` passed all 266 tests on Windows (235 Rust, 16 frontend, 2 server-registration and 13 development-controller tests). The later fixture change was checked locally with `cargo test` and Clippy for `persephone-test-matrix`. At `04c0538`, every CI job passed; see [continuous integration](#continuous-integration).

After merging sign-out and failed-window retry, `python tools/dev.py check --offline` passed all 266 tests (235 Rust, 16 frontend, 2 server-registration and 13 development-controller tests).

After the failed-window review fixes, `python tools/dev.py check --offline` passed all 257 tests (226 Rust, 16 frontend, 2 server-registration and 13 development-controller tests); the first version passed 256 (225 Rust, 16 frontend, 2 server-registration and 13 development-controller tests). See the [window retry evidence](evidence/window-retry.md).

After the second round of sign-out review fixes, `python tools/dev.py check --offline` passed all 258 tests (227 Rust, 16 frontend, 2 server-registration and 13 development-controller tests); the first round passed 255 (224 Rust, 16 frontend, 2 server-registration and 13 development-controller tests); the first version passed 254 (223 Rust, 16 frontend, 2 server-registration and 13 development-controller tests). See the [sign-out evidence](evidence/sign-out.md).

After the sign-in review fixes (commit `a1a1b83`, clean tree), `python tools/dev.py check --offline` passed all 249 tests (218 Rust, 16 frontend, 2 server-registration and 13 development-controller tests); the core's unit tests now also run with the development feature.

After adding native sign-in and real rooms, `python tools/dev.py check --offline` passed all 227 tests (197 Rust, 15 frontend, 2 server-registration and 13 development-controller tests), including the mock-homeserver account tests with the development feature. See the [sign-in evidence](evidence/matrix-sign-in.md) for the live runs.

After adding the session owner and its review fixes (commit `089443e`), `python tools/dev.py check --offline` passed all 205 tests (178 Rust, 12 frontend, 2 server-registration and 13 development-controller tests); the first version passed 204 (177 Rust, 12 frontend, 2 server-registration and 13 development-controller tests). See the [session owner evidence](evidence/session-owner.md).

After adding the protected local store, `python tools/dev.py check --offline` passed all 196 tests (169 Rust, 12 frontend, 2 server-registration and 13 development-controller tests). See the [protected store evidence](evidence/protected-store.md).

After adding live room updates, `python tools/dev.py check --offline` passed all 186 tests (159 Rust, 12 frontend, 2 server-registration and 13 development-controller tests). See the [live updates evidence](evidence/live-updates.md); `persephone --demo-live` posts synthetic messages to the planning room.

After adding the bounded room and timeline views, `python tools/dev.py check --offline` passed all 175 tests (152 Rust, 8 frontend, 2 server-registration and 13 development-controller tests). See the [views evidence](evidence/mock-views.md).

After separating the core from the UI, `python tools/dev.py check --offline` passed all 162 tests (142 Rust, 5 frontend, 2 server-registration and 13 development-controller tests). See the [IPC transport evidence](evidence/ipc-transport.md#after-the-coreui-split) for the end-to-end and flood measurements.

After adding the Tauri shell, `python tools/dev.py check --offline` passed all 154 tests (134 Rust, 5 frontend, 2 server-registration and 13 development-controller tests). `check` and `rust` now build the frontend first, because the shell embeds it at compile time. The Tauri dependency graph made `aws-lc-sys` rebuild; that run needed the `AR_x86_64_pc_windows_msvc` workaround below. See the [Tauri shell evidence](evidence/tauri-shell.md).

After adding the contained media worker and broker, `python tools/dev.py check --offline` passed all 144 tests (126 Rust, 3 frontend, 2 server-registration and 13 development-controller tests). The broker's end-to-end tests launch the real worker under AppContainer, Job and process mitigations; they need the workspace build, because `cargo test -p persephone-media-broker` alone does not build the worker binary. See the [media worker evidence](evidence/media-worker.md).

After adding the WebAssembly decoder sandbox, `python tools/dev.py check --offline` passed all 132 tests (114 Rust, 3 frontend, 2 server-registration and 13 development-controller tests). `check` now builds and lints the `wasm32-unknown-unknown` guest first. Twenty-two new tests cover the decoder and the sandbox; see the [WebAssembly decoder evidence](evidence/wasm-decoder.md). Intermediate runs failed on a host stack overflow (fixed by selecting `portable-dispatch`) and two Clippy lints.

After adding the TLS 1.3-only client policy, `python tools/dev.py check --offline` passed all 110 tests (92 Rust, 3 frontend, 2 server-registration and 13 development-controller tests). Five tests in `tests/matrix/tests/tls_policy.rs` check the policy on the wire; see the [dependency review](evidence/dependencies.md#compiled-graph-windows-x64). An intermediate run failed Clippy on two parser lints; both were fixed before the passing run.

After recording the dependency maintenance disposition, `python tools/dev.py check --offline` passed all 105 tests (87 Rust, 3 frontend, 2 server-registration and 13 development-controller tests). Two new tests in `tests/matrix/tests/dependency_contracts.rs` guard the retained `anymap2` crate; see the [dependency review](evidence/dependencies.md#maintenance-disposition-and-compiled-graph-review).

After adding the Windows network differential, `python tools/dev.py check --offline` passed all 98 tests (80 Rust, 3 frontend, 2 server-registration and 13 development-controller tests), formatting, Clippy, frontend checks/build and generated-contract checks. Thirteen new Windows tests cover the probe encodings and the denial classifiers. `python tools/dev.py sandbox --offline` exits 1 with a decisive negative network result; see the [Windows sandbox evidence](evidence/windows-sandbox.md#network-denial-differential). No firewall policy was changed, no non-loopback listener was opened, and no homeservers were started.

After adding retained lifecycle ownership, `python tools/dev.py check --offline` passed all 85 tests (67 Rust, 3 frontend, 2 server-registration and 13 development-controller tests), formatting, Clippy, frontend checks/build and generated-contract checks. Twelve new Rust tests cover task ownership and controlled SDK barriers. The [lifecycle report](evidence/sdk-lifecycle.md) records the SDK completion gaps that keep the full drain gate open; the [architecture contract](architecture/sdk-lifecycle.md) defines the remaining work. No live homeservers were started for this step.

The earlier [capability fixture](evidence/capability-fallback.md) passed 30 controlled cases against both servers and a separate genuinely classic-only Synapse case. Use `python tools/matrix_capabilities.py <ignored-native-server-profiles> <new-ignored-run-directory>` after building `verified_recipients`. Start the WSL server runner with `--server synapse --synapse-classic-only` for the real disabled deployment; use `verified_recipients <one-server-profile> <new-report> --discover-classic-only` to check it.

The earlier [encrypted-store restart fixture](evidence/encrypted-restart.md) remains separate evidence for key/trust recovery and resumed native messaging. Its controller is `python tools/matrix_restart.py <ignored-server-profile> <new-ignored-run-directory>`. Both prior [native encrypted sync/classic-transition](evidence/encrypted-sliding-sync.md) and forced classic-only interfaces remain available: `verified_recipients <profile> <result> --sliding-sync`, or omit the flag for forced classic-only. These live scenarios were not rerun for this lifecycle step.

On Windows, finish live executable runs before starting Cargo commands that may replace that executable. Check WSL listeners from Windows before a long suite. The Windows containment gate remains unresolved and was not rerun for this Matrix change. The [ledger](evidence/verification.json) distinguishes current checks from historical results; individual [evidence reports](README.md#evidence) record their scope and limitations.
