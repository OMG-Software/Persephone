# Third-party module report

Checked 16 November 2026 on Windows 11 Pro x64 (build 26300), debug builds,
rustc 1.99.1, Node from the locked frontend dependencies. Source: branch
`claude/injected-dll-report` on `7b3e00d` plus this change, uncommitted during
the first checks, worktree `.local/wt-report`,
`CARGO_TARGET_DIR=.local/build/rust-wt`, `--locked --offline`. After the
rebase described under [Checks run](#checks-run), the change was committed as
`cfe450f`. The [review fixes](#review-fixes) were checked on `cfe450f` plus
those fixes, and committed after the checks. `Cargo.lock` is unchanged.
**Result: every check that was run passed. `python tools/dev.py check`, the
native containment gate and the dependency audit were not run. The demo app
ran for several minutes with no window failure. The report and the notice
were not seen in the running app, and the scan thread was not observed.**

Decision: [ADR-0044](../adr/0044-report-code-loaded-into-persephone-and-ask-the-user-to-review-it.md).

## What was built

* **Scanner** (`crates/core/src/platform/windows/modules.rs`):
  * Lists each target's modules with `K32EnumProcessModulesEx` (at most 1024
    modules per process) and `K32GetModuleFileNameExW`.
  * Checks each signature with `WinVerifyTrust`: the embedded signature, then
    the SHA-256 and SHA-1 system catalogs, at most 8 catalogs.
  * Uses no UI, `WTD_REVOKE_NONE`, and `WTD_REVOCATION_CHECK_NONE |
    WTD_CACHE_ONLY_URL_RETRIEVAL`. Guards close every trust state and release
    every catalog context.
  * Reads the leaf signer's simple name before the state closes.
  * Caches results by path, size and write time, at most 2048 entries.
  * Gets the Windows folder from `GetSystemWindowsDirectoryW`, not from
    `%SystemRoot%`.
* **UI Job**: `UiTree::process_ids` uses
  `QueryInformationJobObject(JobObjectBasicProcessIdList)`, at most 256 IDs.
* **Core state** (`crates/core/src/modules.rs`):
  * Builds the review keys, merges a file's processes, sorts the rows and caps
    the report (128 rows, 96 KiB serialized, 4096 distinct files per scan).
  * Projects the text: names and signers like display names; paths with
    control and bidi characters replaced, cut to 1024 bytes.
  * Stores the reviewed keys under `persephone.reviewed_modules.v1` in the
    profile's crypto store, at most 200 keys and 64 KiB. A bad record reads as
    none reviewed.
* **Core integration** (`crates/core/src/lib.rs`, `main.rs`):
  * A scan thread per window run, using the scanner across runs.
  * The `ModulesChanged` event.
  * The `third_party_modules` and `acknowledge_modules` answers. The second
    checks its limits again, holds a lock across the stored read and write,
    and replies `internal` if the save fails.
* **Wire contract**: the module types in `persephone-contracts` and the
  generated TypeScript, the two calls, the event, and the id limits in
  `personal_call_within_limits`.
* **UI host**: the two commands relay through `personal_relay`, with
  capability entries and generated permission files. `ModulesChanged` goes to
  the page as `"modules"`.
* **Page**:
  * `LoadedCode.svelte` in both Settings > Security branches.
  * The notice in the signed-in main view.
  * `modules.ts` helpers.
  * The bridge calls and the fourth `watchSession` callback.

No module path, name or signer is printed or logged.

## Tests

New:

* `persephone-contracts`: `module_report_round_trips_and_requires_explicit_nulls`.
* `persephone-core-protocol`: `given_module_calls_when_limits_are_checked_then_bounds_hold`, and rows in the call round-trip and event wire tests.
* `persephone-core`, scanner:
  * `expected_modules_follow_the_rules` (18 cases);
  * `a_self_scan_omits_microsoft_system_modules`;
  * `an_unreadable_process_is_skipped`;
  * `kernel32_has_a_valid_microsoft_signature`;
  * `a_tampered_microsoft_dll_is_reported`. This is the negative control: a
    copy of `version.dll` with one DOS-stub byte changed is reported as
    `unsigned` or `invalid`, with no signer.
* `persephone-core`, UI Job: `given_a_job_with_a_child_then_its_pid_is_listed`.
* `persephone-core`, state: twelve `given_…` tests in `modules.rs`. They cover:
  * review keys, merging, sorting and the caps;
  * hostile text, path prefixes and unknown keys;
  * stored keys, the 200-key cap and change detection;
  * the scan states and stored-record limits.
* `persephone-core`, end to end: `given_a_module_scan_then_events_reports_and_reviews_follow`.
* `persephone-desktop`: `module_commands_work_from_the_app_origin_and_refuse_bad_ids_before_relay`, and a `"modules"` case in the dispatch test.
* Frontend:
  * `modules.test.mjs` (six tests);
  * a `LoadedCode` render test with a hostile file name;
  * bridge rows for both calls;
  * a session-watch test showing that only an unknown message resets the
    session.

Updated: `given_shell_config_then_security_settings_are_restrictive` lists the
two new permissions.

## Checks run

All exited with status 0, on 16 November 2026, in the order shown:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | passed |
| `cargo test --locked --offline -p persephone-contracts -p persephone-core-protocol` | 7, 16 and 25 passed |
| `cargo test --locked --offline -p persephone-core` | lib 281 passed, 1 ignored; integration binaries 13, 5, 5, 2, 12, 15, 20 and 4 passed |
| `cargo test --locked --offline -p persephone-desktop` | 15 passed, 1 ignored; IPC 35 passed |
| `cargo clippy --locked --offline -p persephone-contracts -p persephone-core-protocol -p persephone-core -p persephone-desktop --all-targets -- -D warnings` | passed |
| `cargo clippy --locked --offline -p persephone-core --features dev-disposable-servers --all-targets -- -D warnings` | passed |
| `cargo run --locked --offline -p persephone-contracts --bin export-contracts -- --check` | bindings current |
| `python tools/dev.py frontend --offline` | svelte-check 176 files, 0 errors, 0 warnings; build passed; 313 Node tests passed |

Before publishing, the change was rebased onto `fa78836`, the head of the
injected DLL hardening branch. That adds only the hardening tests' system-path
fix and evidence text. Rerun after the rebase, all passing: `cargo fmt --all
-- --check`; `cargo test --locked --offline -p persephone-core --lib -- harden
modules` (19 passed, 1 ignored); `cargo test --locked --offline -p
persephone-desktop` (15 passed, 1 ignored; IPC 35 passed); `cargo clippy
--locked --offline -p persephone-core -p persephone-desktop --all-targets --
-D warnings`.

The worktree's `frontend/node_modules` is a junction to the main checkout's
locked install. No package was installed.

## Review fixes

Made on 16 November 2026 after the pull request review and the first CI run.
This is a later change and a separate run.

* **CI:** five `modules.rs` tests failed on Linux and macOS, because
  `Path::file_name` does not split a Windows path there. The file name now
  comes from the last part after `\` or `/` on every host.
* **Incomplete scans:** the report has a new `incomplete` field. It is set
  when the UI Job's process list cannot be read, when a target process cannot
  be opened or listed and has not exited, when a process has more than 8192
  modules, or when a module path does not fit. The module list call now grows
  its buffer from the size it reports, up to 8192 handles in three tries.
  Settings shows "Some Persephone processes could not be checked" instead of
  "No other code is loaded".
* **Reviews per profile:** while a profile store is open, the reviewed keys
  in memory are replaced by the store's on each read, and a lock or sign-out
  clears them. Reviews of one profile, or from before sign-in, no longer show
  for another profile or reach its store.
* **Scan thread:** it is joined at the end of the run, so a panic in it cannot
  reach the core's shutdown path.
* **Page:** a session change clears the report until the new one arrives. A
  review takes its request number before it waits, so a newer refresh is not
  dropped.

New tests: `given_other_reviews_then_the_stored_ids_replace_them`,
`given_an_incomplete_scan_then_the_report_says_so`,
`given_either_separator_then_the_file_name_is_the_last_part`,
`a_process_that_has_gone_is_skipped` (renamed from
`an_unreadable_process_is_skipped`, now also checking completeness) and
`an_unreadable_process_makes_the_scan_incomplete`. The second scanner test
uses the System process (pid 4), which a user process cannot open for
reading, with the test process alone as its control. Updated: the
end-to-end module test also locks and checks the reviews are cleared and the
flag passes through; the contract, TypeScript contract, IPC fixture and page
tests carry the flag.

Checks run, all exited with status 0:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | passed |
| `cargo test --locked --offline -p persephone-contracts -p persephone-core-protocol` | 7, 16 and 25 passed |
| `cargo test --locked --offline -p persephone-core` | lib 285 passed, 1 ignored; integration binaries 13, 5, 5, 2, 12, 15, 20 and 4 passed |
| `cargo clippy --locked --offline -p persephone-contracts -p persephone-core-protocol -p persephone-core -p persephone-desktop --all-targets -- -D warnings` | passed |
| `cargo run --locked --offline -p persephone-contracts --bin export-contracts -- --check` | bindings current |
| `cargo test --locked --offline -p persephone-desktop` | 15 passed, 1 ignored; IPC 35 passed |
| `python tools/dev.py frontend --offline` | svelte-check 176 files, 0 errors, 0 warnings; 313 Node tests passed |

The first desktop run failed one IPC test, because its fake core reply lacked
the new field and the UI host refused it. The fixture was fixed and the IPC
tests rerun. The Linux and macOS fix was not run locally: only CI runs
those hosts.

Second round, after the review of `1e03d88`:

* A Job process list that did not fit (`ERROR_MORE_DATA`, more than 256
  processes) now marks the scan incomplete. `UiTree::process_ids` returns the
  listed IDs and whether they are all of them.
* A module whose path cannot be read now marks the scan incomplete, because
  an unloaded module and an unreadable one cannot be told apart.
* A sign-out or lock leaves the store `RestartRequired`, and a profile switch
  starts a new core process, so a second profile never opens in the same
  core. The new test checks the path that can happen: a lock clears the
  reviews from memory, and a restart on the same profile reads them back from
  its encrypted store.

New tests: `given_more_processes_than_room_then_the_list_is_incomplete` (a
Job with `cmd` and its `ping` child, listed with room for one; the full list
is the control) and
`given_a_lock_then_module_reviews_leave_memory_and_return_from_the_store`
(mock homeserver, `dev-disposable-servers`). Negative control: with the
clear-on-lock line removed, the second test failed at its "locked" check;
the line was then restored unchanged.

Checks run on `1e03d88` plus these fixes, all exited with status 0:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | passed |
| `cargo test --locked --offline -p persephone-core` | lib 286 passed, 1 ignored; integration binaries 13, 5, 5, 2, 12, 15, 20 and 4 passed |
| `cargo test --locked --offline -p persephone-core --features dev-disposable-servers --test account_settings` | 14 passed |
| `cargo test --locked --offline -p persephone-desktop` | 15 passed, 1 ignored; IPC 35 passed |
| `cargo clippy --locked --offline -p persephone-contracts -p persephone-core-protocol -p persephone-core -p persephone-desktop --all-targets -- -D warnings` | passed |
| `cargo clippy --locked --offline -p persephone-core --features dev-disposable-servers --all-targets -- -D warnings` | passed |

The frontend and contract checks were not rerun: this round changed no
contract or page file.

## Live run

`persephone.exe --demo` from this build, with `WEBVIEW2_USER_DATA_FOLDER` set
to a disposable folder under `.local`:

* The window started and showed the synthetic demo data. The core's error
  output stayed empty, and no window failure was reported, for several minutes.
  That is well past the 10 s limit for the first scan. Whether a scan ran was
  not observed, because nothing about modules is logged.
* The demo runs without an account, so the notice is hidden, as designed.
* Settings could not be opened by UI Automation: the menu closed before the
  item was invoked. The Security list was therefore not seen live. Its
  rendering is covered only by the server-side render test.
* Stopping the core left no process of the run.

## Limits

* Not seen live: the Security list, the notice, an acknowledgement, the saved
  reviews in a real profile, and the `ModulesChanged` event from the real scan
  thread.
* The reviewed keys are tested against a real profile store only through a
  lock and restart of one profile. A second profile in one core process is
  not tested, because the store then needs a restart.
* Not run: `python tools/dev.py check`, the native containment gate
  (`python tools/dev.py sandbox --offline`) and the dependency audit
  (`python tools/dev.py audit`).
* The incomplete flag was not seen for a real WebView2 process. Whether
  WebView2's sandboxed processes can be opened for reading was not checked
  live.
* The scan thread has no unit test. The scanner, the state and the answers
  are tested separately.
* The report shows what the core can see. Code that is already loaded can hide
  from it. This is transparency, not a boundary against same-user malware.
* Reviews made while no profile is open last only for that core process. They
  stay in memory after a sign-out until the core exits.
* Windows only. Other platforms report "unsupported".
* Debug builds on one Windows host only.
