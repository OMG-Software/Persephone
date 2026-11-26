# Development

## Repository layout

The repository groups files by their role. The root Cargo workspace and lockfile cover the shared crates and Rust integration/security test programs. The application starts as `persephone` (the core, `crates/core`), which launches the keyless UI `persephone-ui` (`src-tauri/`) and serves its requests (ADR-0004). Run the app with `.local/build/rust/debug/persephone.exe` after a build. Started directly, `persephone-ui` starts `persephone` next to it with no arguments and exits (`src-tauri/src/launcher.rs`); installed shortcuts work this way. With no arguments, the core opens (or creates) the encrypted default profile (ADR-0013): `%LOCALAPPDATA%\OMG-Software\Persephone\profile` on Windows, `~/Library/Application Support/OMG-Software/Persephone/profile` on macOS, and `$XDG_DATA_HOME/persephone/profile` (else `~/.local/share/persephone/profile`) on Linux. Add `--profile <dir>` to use another directory; use a disposable directory under `.local/` for development profiles. Add `--account <n>` (2 to 8) to open the profile in slot `n` beside the base one, such as `profile-2` (ADR-0015); Settings, Accounts, adds and switches profiles by restarting the core. Settings, Preferences, are kept per account in its encrypted profile: Enter to send, the message and interface text sizes, the colour theme (one of three dark themes; the default shows before the profile opens) and desktop notifications; the notification level there is the account's push rules on the homeserver (ADR-0022). Settings, Account, changes the password and the email addresses and phone numbers (ADR-0019, ADR-0020); the core asks for passwords and phone codes in its own windows. Settings, Security, changes the recovery key and saves a new key with the core's own Save dialog (ADR-0021); Settings, Sessions, verifies another of the account's devices from a verified one. With `PERSEPHONE_DEV_PROFILE`, the harness profile's `password`, optional `new_password` and optional `phone_code` answer those prompts; the recovery-key warning and Save dialog stay native. The settings button in a room's header opens that room's settings (ADR-0017). "Create an account" on the sign-in page registers a new account (ADR-0018); with `PERSEPHONE_DEV_PROFILE`, the harness profile's `password` and optional `registration_token` answer its prompts. Add `--demo` instead to see the synthetic rooms without a profile; nothing is stored, and `--demo-live` (only with `--demo`) adds a synthetic message every few seconds.

```text
Cargo.toml                         # Rust workspace
Cargo.lock                         # One committed Rust dependency resolution
.cargo/config.toml                 # Cargo output under .local/build/rust
src-tauri/                         # Keyless Tauri UI (persephone-ui), capabilities, window policy
crates/
  core/                            # Key-holding core; entry binary `persephone`
  core-protocol/                   # UI-to-core messages (bounded frames)
  contracts/                       # Shared DTO and contract-generation code
  media-broker/                    # Launches the contained media worker (Windows, Linux, macOS)
  media-decoder/                   # PNG/JPEG decoder built as a wasm32 guest
  media-protocol/                  # Shared byte-budget and framed protocol code
  media-sandbox/                   # wasmi host: import allowlist and resource limits
  media-worker/                    # Contained worker binary; embeds the decoder guest
  screen-helper/                   # Windows screen capture helper outside the call worker (ADR-0067)
tests/
  matrix/                          # Matrix sync and encrypted-message integration tests
  storage/                         # Encrypted-store and credential API tests
  platform/windows/                # Windows containment test program
  tools/, servers/                 # Python tests for the tools and server harness
frontend/                          # Bundled Svelte UI and typed host bridge
tools/
  dev.py                           # Repository check, build and run entry point
  matrix_restart.py                # Disposable Rust worker restart controller
  matrix_capabilities.py           # Controlled sync-capability integration runner
  audit_dependencies.py            # OSV Rust dependency audit
  release.py                       # Release version, tag check and bundle configuration
  sign_windows.ps1                 # Windows signing step of the release workflow
  about.toml, about.hbs            # cargo-about third-party notice configuration
  servers/                         # Disposable Merovingian/Synapse harness
docs/
  adr/                             # Accepted and proposed decisions
  architecture/                    # Architecture and security plans
  evidence/                        # Run records and verification artifacts
  planning/                        # Project plan and feasibility status
  research/                        # Framework security research
site/                              # Static project page (GitHub Pages)
.github/workflows/                 # CI, release, SonarQube and Pages workflows
.local/
  cache/cargo/                     # Cargo download cache
  cache/npm/                       # npm cache
  build/rust/                      # Cargo build output
  build/frontend/                  # Frontend build output
  runs/                            # New synthetic test profiles and output
  servers/merovingian/             # Local Merovingian helper binary
  servers/synapse/                 # Isolated Synapse virtual environment
```

The `contracts` and `media-protocol` crates are shared building blocks. They remain provisional and are not security-approved application components. The integration and platform programs exercise bounded scenarios; their results do not establish production protections or close the P0 phase. The frontend builds static assets that the desktop shell embeds.

Historical evidence reports preserve command paths, source hashes, and package-resolution facts from before this layout was created. Those old paths identify the recorded run inputs. The [evidence index](README.md#evidence) links the reports; use this guide for current locations and commands.

## Room-tool ownership

`crates/core/src/personal.rs` keeps bounded drafts and room organisation in the
encrypted crypto store. `crates/core/src/local_search.rs` reads the encrypted
SDK event cache without HTTP or a separate index. `crates/core/src/abuse.rs`
uses standard Matrix ignore account data and event reports. The core owns all
store, key and SDK operations; Tauri and the frontend relay typed commands and
show inert text. Personal-record writes and ignore-list writes need serialized
mutation gates to prevent lost updates.

Personal calls retain a session lease at admission and fence their final reply
with `Lease::commit`. Search and abuse operations have separate Busy gates.

The SDK event cache must subscribe before sync to retain cache events. This
subscription adds SDK background work. It does not establish stop-and-join
ownership. The existing `restart_required` lifecycle limitation remains. See
[Room tools](room-tools.md), its [verification record](evidence/room-tools.md),
and the [SDK lifecycle contract](architecture/sdk-lifecycle.md).

## Toolchain and commands

Install Python, Rust/Cargo 1.99 or later, and Node.js with npm available on `PATH`, plus the WebAssembly target for the decoder guest (`rustup target add wasm32-unknown-unknown`). Cargo refuses older toolchains through the workspace `rust-version`, because their std breaks the contained image decoder: the guest cannot allocate in its fixed memory before 1.97, and the Linux worker calls `sigaltstack` at exit, which its seccomp allowlist kills, before 1.99. See the comment on `rust-version` in the root `Cargo.toml`. On Windows, use the MSVC Rust target and its native build tools. On Linux, the call worker needs clang 21 or later (`clang++`, or `CXX`), because the pinned libwebrtc is built against a libc++ that needs it. With an older clang or none, `check`, `rust`, `build` and `coverage` skip the call worker and print why. The worker does not ship on Linux and calls are refused there (ADR-0043). With `PERSEPHONE_CALL_WORKER=required`, as CI sets it, they fail instead of skipping. Install the locked frontend dependencies once, then run checks from the repository root:

```powershell
python tools/dev.py bootstrap
python tools/dev.py check
python tools/dev.py build
python tools/dev.py build --release
python tools/dev.py rust
python tools/dev.py frontend
python tools/dev.py contracts
python tools/dev.py coverage
python tools/dev.py audit
python tools/dev.py sandbox --offline
python tools/dev.py webrtc
```

`webrtc` downloads the pinned libwebrtc archive from `tools/webrtc.json`, verifies its size and SHA-256, and extracts it into `.local/cache/webrtc` for the call worker (ADR-0043). It prints the directory for `LK_CUSTOM_WEBRTC`. It needs network access unless the verified archive is already cached. With `--offline` it fails if nothing is cached.

`coverage` runs the Rust tests instrumented and writes the Rust crates' line coverage to `.local/coverage/rust/lcov.info`, as the SonarQube workflow does. Instrumented builds go to `.local/build/rust-coverage`. It needs the LLVM tools component and cargo-llvm-cov 0.9.1 on `PATH`; `dev.py` refuses another version:

```powershell
rustup component add llvm-tools
cargo install --locked --root .local/tools/cargo-llvm-cov cargo-llvm-cov --version 0.9.1
```

Then add `.local/tools/cargo-llvm-cov/bin` to `PATH`. The contained media worker cannot write a coverage profile, so it runs uninstrumented and its own package's tests are left out of `coverage`; `check` still runs them.

On Linux, `coverage --linux-live` also runs the core's ignored live tests, instrumented: the UI scope tests against the systemd user manager on the session bus, then the Secret Service test on a private bus (`dbus-run-session`) with a new GNOME Keyring, unlocked with a throwaway password, whose data directory is a temporary one under `.local/live` and whose runtime directory is a new private temporary one, so it cannot find or unlock a keyring that is already running. Both runs drop `GNOME_KEYRING_CONTROL`, which GNOME Keyring reads before `XDG_RUNTIME_DIR`. A second new GNOME Keyring, started without `--unlock` and without `DISPLAY` or `WAYLAND_DISPLAY`, has no default keyring; the create-keyring refusals of ADR-0061 run against it. It needs `dbus-run-session` and `gnome-keyring-daemon`. The SonarQube workflow uses it on its disposable runner, where it first starts a user manager by lingering. The scope tests start real transient scopes in the user manager they reach, so use the option only on a disposable machine or user.

`check` runs Rust formatting, workspace tests and Clippy, frontend check/build/tests (the tests write the frontend TypeScript's line coverage to `.local/coverage/frontend/lcov.info`; the SonarQube workflow runs `frontend` for it), generated-contract drift/TypeScript checks, disposable-server registration tests, and restart-controller tests. It does not run live homeservers, the native containment program or dependency audit. `audit` queries OSV and needs network access; it cannot run offline. Add `--offline` to supported Cargo, frontend, contract, bootstrap, or sandbox commands when using cached dependencies.

`build` compiles the application without running tests: the frontend, the release decoder guest (the media worker embeds it in every profile), then `persephone`, `persephone-media-worker`, `persephone-screen-helper` (a stub off Windows), `persephone-call-worker` (with the static C runtime on Windows; skipped on Linux when clang is older than libwebrtc needs) and `persephone-ui`. Without `--release` the executables go to `.local/build/rust/debug`; with it, to `.local/build/rust/release`. A target set in `CARGO_BUILD_TARGET` adds the target's name to that path. Start the app with `persephone.exe` (or `persephone`) from that directory. On Linux and macOS the app has never been started with a display, so treat a build there as untested (see [Continuous integration](#continuous-integration)). On Linux the profile needs a Secret Service provider (such as GNOME Keyring or KWallet) with a default keyring (ADR-0010). A desktop that logs in automatically may have none: the core then asks in its own window whether the keyring may create one, and the keyring asks for its new password in its own prompt (ADR-0061, proposed). Without a session bus, a provider, a default keyring or a display, the profile stays locked and the page names the missing part. `build` uses the default features, so it accepts only `https://` homeservers (see [Signing the app in to a disposable server](#signing-the-app-in-to-a-disposable-server)). It does not make installers or sign anything; see [release builds](#release-builds).

The Windows containment run is deliberately separate. On Windows it currently exits 1 because the zero-capability AppContainer worker is not network-denied: an endpoint-observed differential shows its packets reach the local TCP stack. The run also prints a comparative LPAC result, which does not change the gate. On other operating systems the command exits 2 because this program requires Windows; equivalent Linux and macOS coverage remains open. The unresolved gate is recorded in the [feasibility workbench](planning/feasibility.md) and [Windows sandbox evidence](evidence/windows-sandbox.md).

For focused Rust work, use `cargo test --locked -p <package>` from the root. Current package names are `persephone-contracts`, `persephone-call-protocol`, `persephone-media-protocol`, `persephone-media-decoder`, `persephone-media-sandbox`, `persephone-media-worker`, `persephone-media-broker`, `persephone-call-worker`, `persephone-screen-helper`, `persephone-core`, `persephone-core-protocol`, `persephone-desktop`, `persephone-test-matrix`, `persephone-test-storage`, and `persephone-test-windows`. All resolve through the root lockfile and use `.local/build/rust` for output.

Run the controlled lifecycle tests with `cargo test --locked -p persephone-test-matrix --test lifecycle_owner --test lifecycle_sdk`. They use synthetic loopback HTTP and memory stores. The Rust prototype tracks registered work; it cannot certify full SDK shutdown or production lock/logout.

If the Windows MSVC `lib.exe` AWS-LC archive step fails, set `AR_x86_64_pc_windows_msvc` to an installed LLVM librarian in the current shell and retry. For example:

```powershell
$env:AR_x86_64_pc_windows_msvc = (Get-Command llvm-lib.exe).Source
```

This is a local toolchain workaround. Do not add a machine-specific LLVM path to repository configuration.

### Continuous integration

[`.github/workflows/check.yml`](../.github/workflows/check.yml) runs `python tools/dev.py bootstrap` and `python tools/dev.py check` (`python3` in the distribution containers). Every job pins Rust 1.99.1 (with `rustfmt`, `clippy` and the `wasm32-unknown-unknown` target) and Node.js 24.15.0 and uses the committed lockfiles. Like the local check, it does not run live homeservers, the native sandbox probe or the dependency audit.

The jobs are tiered by event ([ADR-0051](adr/0051-tier-ci-by-event-and-run-the-full-matrix-in-the-merge-queue.md)):

- **Pull request:** `windows` and `ubuntu-latest` run. The other six jobs are skipped, and a skipped job reports success. A newer push to the pull request cancels the older run.
- **Merge queue (`merge_group`), push to `main`, manual run:** all eight jobs run, and no run is cancelled. A branch without a pull request is run by hand with `workflow_dispatch`.
- **Required checks:** each job name in the table below is a required check of the "Protect main" ruleset. Branch protection matches names exactly, so no job that reports one is a matrix. The shared steps are YAML anchors. Keep the ruleset, the job names and `REQUIRED_CHECKS` in `tests/tools/test_workflows.py` the same.
- **Caches:** every job restores the downloaded Cargo and npm sources and libwebrtc; only `main` saves them. `windows` and `ubuntu-latest` also restore compiled dependencies (rust-cache, saved from `main` only) and write the hit and the target size to the job summary.
- **Workflow tests:** `tests/tools/test_workflows.py` checks these rules for every workflow. It needs PyYAML and is skipped without it, except in `ubuntu-latest`, which installs PyYAML from [`tools/ci-requirements.txt`](../tools/ci-requirements.txt) (pinned by hash) and sets `PERSEPHONE_WORKFLOW_TESTS=required`.

| Job | Runner or image | Prerequisites | Python |
| --- | --- | --- | --- |
| `windows` (pull requests too) | `windows-latest` | Preinstalled MSVC | 3.14 (setup-python) |
| `macos-latest` | `macos-latest` (arm64) | Preinstalled Xcode tools | 3.14 (setup-python) |
| `ubuntu-latest` (pull requests too) | `ubuntu-latest` (26.04 on 17 November 2026) | `apt-get`: `libwebkit2gtk-4.1-dev build-essential pkg-config clang` | 3.14 (setup-python) |
| `ubuntu-24.04-arm` | `ubuntu-24.04-arm` (arm64) | As `ubuntu-latest`; rustup from `sh.rustup.rs` if the image lacks it. Clang 21 from apt.llvm.org, because the image has clang 18 | 3.14 (setup-python) |
| `debian-13` | `debian:trixie` container | `apt-get`: build tools, `clang`, `libwebkit2gtk-4.1-dev`. Clang 21 from apt.llvm.org, because Debian has clang 19 | Distribution `python3` |
| `arch` | `archlinux:base-devel` container | `pacman -Syu`: `clang`, `webkit2gtk-4.1` | Distribution `python` |
| `fedora-44` | `fedora:44` container | `dnf`: build tools, `clang`, `pkgconfig(webkit2gtk-4.1)` | Distribution `python3` |
| `opensuse-leap-16.0` | `opensuse/leap:16.0` container | `zypper`: build tools, `pkgconfig(webkit2gtk-4.1)`. Clang 21 only, from `devel:tools:compiler`: Leap has clang 19, whose `libclang13` conflicts with clang 21's | Distribution `python3` |

- **Containers.** Container images are pinned by digest in the workflow. Containers install Rust with rustup from `sh.rustup.rs` and Node.js with setup-node.
- **Packages are not pinned.** Packages come from each distribution's current repositories, so only the base image is fixed. Arch supports only full upgrades, so its job always builds against the current rolling release.
- **Clang 21 for the call worker.** The pinned libwebrtc's libc++ needs clang 21 or later. Where a job's clang is older, [`.github/scripts/install-clang.sh`](../.github/scripts/install-clang.sh) adds one extra repository and installs clang 21 from it; elsewhere it does nothing. It trusts each repository only through a public signing key committed in `.github/keys`, and downloads no key:
  - `apt.llvm.org.asc`, LLVM's apt repository (`llvm-toolchain-<codename>-21`) for Debian and Ubuntu: fingerprint `6084 F3CF 814B 57C1 CF12 EFD5 15CF 4D18 AF4F 7421`, no expiry.
  - `devel-tools-compiler.asc`, the openSUSE Build Service project `devel:tools:compiler` for Leap: fingerprint `CB3E D75A C0E1 FE7B 52F5 58E2 0FD0 7641 6330 7D5D`, expires 13 April 2028. Replace it with the project's renewed key before then.
  Both keys were taken from the repositories on 17 November 2026 and checked with `gpg --show-keys`. Only clang comes from these repositories: openSUSE gives `devel:tools:compiler` a lower priority than its own repositories.
- **Off Windows, CI covers build and tests only.** Linux and macOS have worker containment (seccomp on Linux, Seatbelt on macOS; ADR-0010, ADR-0011), and its probe and contained-worker tests run in their jobs. macOS also has its Keychain vault, prompts, image picker and UI host (CI-tested only, never run with a display). Its file panels, link opening and idle time (13 November 2026) have not yet been through macOS CI. Linux also has its Secret Service vault, GTK prompts and file choosers, and UI host with a systemd scope (ADR-0010); its live Secret Service and scope tests are ignored by `check` and run only in the SonarQube coverage job (`coverage --linux-live`, Ubuntu) and by hand in WSL ([dependency evidence](evidence/dependencies.md#linux-platform-layer-gtk-and-zbus-in-the-core-13-november-2026)), and its windows have never been shown. No job launches the application on any OS.
- **Tests that compile on one OS only.** The AppContainer probe, DPAPI and the native Windows prompts run only on Windows; seccomp probes only on Linux; Keychain, AppKit and Seatbelt tests only on macOS. Test counts therefore differ between jobs.

Other workflows: [`sonar.yml`](../.github/workflows/sonar.yml) sends the source to SonarQube Cloud for static analysis on pushes to `main` and same-repository pull requests. Its `coverage` job, which has no secret, runs the frontend and instrumented Rust tests and uploads the LCOV reports; its `sonarqube` job holds `SONAR_TOKEN` and only downloads the reports and runs the scan. [`pages.yml`](../.github/workflows/pages.yml) deploys `site/`.

Run 39785251911 at `04c0538` (22 October 2026) passed every job it had. The `ubuntu-24.04-arm` job was added later and has no recorded result here:

| Jobs | Rust tests (workspace + development feature) | Frontend | Python |
| --- | --- | --- | --- |
| `windows` | 235 (202 + 33) | 16 | 15 (2 + 13) |
| `macos-latest`, `ubuntu-latest`, `debian-13`, `arch`, `fedora-44`, `opensuse-leap-16.0` | 194 (163 + 31) each | 16 each | 15 (2 + 13) each |

On every job, `cargo fmt`, Clippy with `-D warnings` (default and development feature), the contract drift check and the TypeScript check also passed. Each job took 8 to 17 minutes. The `#[cfg(unix)]` directory sync in `crates/core/src/store.rs` compiles on Linux and macOS, and the sign-out and store tests that use it pass there.

### Release builds

[`.github/workflows/release.yml`](../.github/workflows/release.yml) builds,
signs and checksums the installers. A `v*` tag creates a draft pre-release; a
manual run builds unsigned artifacts only. Each push to `main` builds the
unsigned files, and the `latest` pre-release is replaced only if that commit
is still the tip of `main` when the build ends. [Release guide](release.md)
lists the secrets, how to cut a release and how users verify the files.

The Linux release jobs build on Ubuntu 22.04, which sets the bundles' glibc
floor. Its PipeWire 0.3.48 headers are too old for `libspa` 0.10.1, which
needs 0.3.65 or later (`spa_meta_region_is_valid` and `spa_meta_first` as
inline functions, and the `flags` field and unsigned `modifier` of
`spa_video_info_raw`). Where the headers are older,
[`.github/scripts/install-pipewire-headers.sh`](../.github/scripts/install-pipewire-headers.sh)
downloads `libspa-0.2-dev` and `libpipewire-0.3-dev` 1.0.5-1 from the Ubuntu
24.04 (noble) release pocket, unpacks only their files, and points
`PKG_CONFIG_PATH` at them; elsewhere it does nothing. The amd64 packages serve
both architectures, because headers do not depend on it. The call worker still
links the system's `libpipewire-0.3.so` 0.3.48, so the link fails if the
worker calls a function that the older library lacks, and the `.deb` and
`.rpm` packages still depend on the system's `libpipewire-0.3-0`. Each
package's SHA-256 in the script is the one in noble's `main/binary-amd64`
`Packages.xz`, which noble's `InRelease` lists and the Ubuntu Archive
Automatic Signing Key (2018), fingerprint
`F6EC B376 2474 EDA9 D21B 7022 8719 20D1 991B C93C`, signs. Both were checked
with `gpgv` and `sha256sum` on 19 November 2026. The release pocket does not
change during noble's life; when noble leaves the archive, move the pins to
another signed release.

The "Minimise to tray" item (ADR-0060) needs Tauri's `tray-icon` feature. At
run time, `libappindicator-sys` loads `libayatana-appindicator3.so.1`, or else
`libappindicator3.so.1`, or else either name without the `.1`; it panics only
if all four fail. The release jobs install `libayatana-appindicator3-dev`
with `apt-get` (no hash pin, like their other packages), because the Tauri
bundler finds the tray library with pkg-config and panics if it is missing.
The bundler then copies the library into the AppImage and adds it to the
package dependencies: `libayatana-appindicator3-1` in the `.deb` and
`libayatana-appindicator3.so.1()(64bit)` in the `.rpm` (tauri-cli 2.12.3).

The app's version line shows the build's revision: the first 12 hex digits of
the checked-out commit, or `unknown` ([`crates/core/build.rs`](../crates/core/build.rs)).
It does not show uncommitted changes. Without a usable Git checkout of this
source tree, the build uses `GITHUB_SHA`.

## Evidence checks

Run `python tools/check_evidence.py --help` for source-manifest, strict JSON
and local inline Markdown-link checks. In current-checkout mode, manifest paths
are repository-relative and a real-path guard rejects paths that resolve
outside the checkout, including through symlinks. In `--revision` mode, paths
are portable lexical repository-relative POSIX paths. The checker rejects
empty, `.` or `..` segments, absolute paths, drive or colon syntax, backslashes
and NUL. It reads blobs from the selected commit; it does not use current
checkout symlinks to resolve historical paths.

Historical revisions resolve to one commit before blob lookup. The checker
rejects Git option arguments and non-commit objects. Strict JSON rejects
non-finite tokens. Markdown checks ignore triple-backtick fences and matching-backtick
code spans. An escaped backtick opener stays literal; spans may contain a
single newline but do not cross a blank paragraph. The checker still checks
real inline links, including links with titles and angle-bracket targets.
Source hashes normalize CRLF to LF.

The tool's whitespace check covers only unstaged tracked changes. Before a
commit, stage the intended files and run `git diff --cached --check` separately.
See the [monitor-fix record](evidence/room-navigation-monitor-fixes.md) for exact
commands, exit statuses, source hashes and remaining acceptance work.

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

The profile passphrase is optional and off by default (ADR-0050). A new profile opens with no prompt, and the OS vault alone wraps the store key. Settings, Security turns the passphrase on or off; the core asks in its own native window (twice, at least 10 characters, when turning it on). A profile that has a passphrase asks for it in the core's native window before the page opens (ADR-0006). Lock and the idle lock show only while the passphrase is on. "Lock" closes the profile; "Unlock" asks for the passphrase again, but after a sign-in only a restart unlocks. "Reset profile…" is shown while the profile is closed; the core confirms in its own window, deletes the profile and starts a new one without a passphrase. With `PERSEPHONE_DEV_PROFILE`, `PERSEPHONE_DEV_PASSPHRASE_FILE` (a path inside `.local`) answers the passphrase prompts from that file (they appear only while the passphrase is on); without it the native window is used. See the [unlock secret evidence](evidence/unlock-secret.md).

"Sign out" in Settings, This session, attempts to revoke the session on the homeserver and deletes the profile's key; the next start deletes the store files. Restart Persephone to sign in again. See the [sign-out evidence](evidence/sign-out.md).

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
