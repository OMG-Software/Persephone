# Failed window detection and retry

Checked 21 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Tauri 2.12.4 with the installed WebView2 runtime. **Result: the core noticed a
failed window both when the web view's processes exited and when the page never
reached it, showed its own dialog, and on Retry replaced the UI while the
session kept running. The intermittent blank windows on this host are crashes
of the WebView2 browser process inside a third-party overlay hook
(`RTSSHooks64.dll`, RivaTuner Statistics Server), not a Persephone fault.**

## Design

Owner decision (21 October 2026): detect the failure and offer a retry.

* **Signs** ([`ui_watch.rs`](../../crates/core/src/ui_watch.rs)), checked every
  500 ms:
  * the page sends no request within 30 s of the UI starting (`NotReady`); or
  * after web view processes appeared in the UI's Job, the UI is left alone in
    it for 3 s (`WebViewGone`). The grace covers a normal close, when the web
    view ends a moment before the UI.
  If the Job cannot be read, only the first sign is judged.
* **Readiness** ([`lib.rs`](../../crates/core/src/lib.rs)): the first valid
  request on a connection marks the page as ready. The page's first request is
  `app_status`, sent as it loads.
* **Dialog** ([`ui_job.rs`](../../crates/core/src/ui_job.rs)): the core, not
  the UI, shows a native `MessageBoxW` with Retry and Cancel. It has no owner
  window, because the UI's window is the one that failed. The core prints only
  the failure code.
* **Retry** ([`main.rs`](../../crates/core/src/main.rs)): the core ends the
  whole UI tree with `TerminateJobObject` and waits up to 10 s for the Job to
  empty, so a new web view does not start while the old one holds its data
  folder. It then reopens its connection state (subscriptions, pending events
  and readiness are dropped) and starts a new UI in a new Job. The session, its
  sync loop, its stores and any sign-in in progress are untouched.
* **Cancel**, or a tree that does not exit in time: the core ends the UI, locks
  and closes the stores as on any exit, and exits with code 1.
* The new `windows-sys` feature `Win32_UI_WindowsAndMessaging` (for
  `MessageBoxW`) is in the locked 0.61.2 release; `Cargo.lock` is unchanged.
  The UI crate gains nothing.

## Tests

New, all passing: six decision tests in `ui_watch.rs` (timeout boundary, a
ready page never fails, the web view gone after the grace, brief gaps, a UI
alone while still starting, an unreadable Job), and one serving-loop test: a
reopened core serves a new connection, is not ready until it sends a request,
and keeps no old subscription.

Updated: the store test `given_another_profiles_key_then_the_stores_refuse_it_and_keep_their_data`
failed in the first full check and in 2 of 30 repeated runs on unchanged
store code. The snapshot before the refused open sometimes still held an empty
write-ahead log and the shared-memory index from the earlier close, and the
refused open removed them. The test now compares durable contents (it ignores
`-shm` files and empty `-wal` files; a write-ahead log with content still
counts) and passed 40 of 40 repeated runs.

## Live runs

The probe ran the development core with a fresh profile. It killed the web
view's browser process (`taskkill /F /T`) or suspended the UI process
(`NtSuspendProcess`), found the core's dialog by window class and process, and
pressed a button with `BM_CLICK`. The dialog's buttons expose no UI Automation
Invoke pattern, because the core has no Common Controls manifest.

| Case | Runs | Result |
| --- | --- | --- |
| Web view killed, Retry | 5 | dialog 3.4–3.8 s after the kill; old UI gone, new UI started, same core process, key file in place. The new page reached the core in the 2 runs that checked it and whose new web view did not crash on its own; in a third, without DevTools, the new web view's 6 processes stayed up for 26 s |
| Cancel | 4 | 2 after a second kill (dialog after 3.4–3.5 s), 2 after a spontaneous crash of the replacement; the core exited 1 each time with no store-close error |
| UI suspended before its web view started, Retry | 1 | dialog 30.6 s after start (`NotReady`); the suspended tree ended and a new UI started |
| Web view crashed on its own (retry runs) | 4 | detected as `WebViewGone`; one was the first window, whose replacement then stayed up for 25 s |
| Normal close (`CloseMainWindow`) with the web view running | 7 (5 with DevTools, 2 without) | exit 0, no dialog, no failure line |
| Normal close planned, but the web view had already crashed on its own | 2 | detected as `WebViewGone` before the close; one dialog was answered Retry by hand during the test, not by the probe |
| WebView2 runtime folder missing | 1 | the UI panicked at start and exited, so the core exited 1; no dialog (a retry would not help) |

## Cause of the spontaneous failures on this host

WebView2's own crash reporter (Crashpad) writes minidumps under
`%LOCALAPPDATA%\io.github.omg-software.persephone\EBWebView\Crashpad\reports`;
the UI's Job suppresses Windows Error Reporting, not Crashpad. Eight complete
dumps were written during these runs (forced kills write none); one more
has no exception record and one is empty. A summary that
reads only the exception, module list and Crashpad annotations, not memory,
shows the same crash in all eight:

* process type `browser`;
* exception `0xC0000005` (access violation, read);
* at `RTSSHooks64.dll+0x1490af`, the hook DLL of RivaTuner Statistics Server,
  which was running with its global hook loader.

The earlier "cause not established" WebView2 failures in the
[sign-in evidence](matrix-sign-in.md#limits) match this symptom, and the owner
saw similar behaviour in other Chromium-based applications on this host. Three
older dumps, from 20 October, are empty and were not analysed.

Of seven retries, three replacement web views also crashed on their own within
about 15 s, against one of about a dozen first starts in this session. The sample is too small to
say whether a forced replacement makes the crash more likely.

On this host, an RTSS application profile for `msedgewebview2.exe` with
application detection off should stop the injection; this was not tested.

## Limits

* Windows only. On other platforms the core has no watch and behaves as before.
* The dialog blocks the watch. If the user closes the blank window while the
  dialog is open, the core waits for the answer before exiting.
* A slow but working start that takes more than 30 s to reach the core is
  offered a retry; choosing Retry restarts a window that would have worked.
* A UI that exits on its own (a crash of the UI process, or a missing runtime)
  ends Persephone as before; it is not retried.
* Retries are not limited in number; each needs the user's answer.
* The fix for the root cause is outside Persephone. Blocking injected
  third-party DLLs in the UI tree (for example with a process mitigation
  policy) could prevent it and would also harden the UI. It is not evaluated.
* Debug builds on one Windows host only.
