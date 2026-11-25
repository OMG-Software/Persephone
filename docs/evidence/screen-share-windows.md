# Screen capture in the call worker's AppContainer (Windows)

Checked 19 November 2026 on Windows 11 Pro x64 (build 26300), release build of
the containment gate, rustc 1.99.1, two 2560x1440 monitors, one interactive
session. Source: branch `claude/call-screen-share`, worktree
`.local/wt-call-screen`, `--locked --offline`. `Cargo.lock` gained one line
(the gate package now depends on the already locked `windows 0.62.2`). This
first run did not record its source revision or lockfile hash. A later run
from an identified commit gave the same results; see
[Rerun from an identified commit](#rerun-from-an-identified-commit).
**Update of 25 November 2026: this result is superseded for the product. GDI
`BitBlt` does capture in the same AppContainer; see
[Measurement of 25 November 2026](#measurement-of-25-november-2026-dxgi-and-gdi).
The sections below are the history of the first measurement.**

**Result: Windows.Graphics.Capture does not run in the call worker's
AppContainer. `CreateForMonitor` fails with `E_ACCESSDENIED` (`0x80070005`)
with the worker's current capabilities and with each candidate capability.
The worker is granted no new capability, no mitigation was changed, and
Windows reports screen sharing as unavailable.**

Decision: [ADR-0053](../adr/0053-share-the-screen-in-calls-on-linux-and-windows.md).
The ADR left this open: "whether WGC runs in the worker's non-packaged
AppContainer, and which capability it needs there, is not known".

## What was measured

`persephone-test-windows --screen-probe`
(`tests/platform/windows/src/screen.rs`) does this:

1. It lists the monitors with the worker's own code
   (`crates/call-worker/src/screen/wgc.rs`, compiled into the gate).
2. **Control:** it runs the capture in the gate process itself (the same user
   session, no container): `CreateForMonitor` on monitor 0, a Direct3D 11
   device, a frame pool, a session, then one frame. This shows that the
   measurement can detect success.
3. It launches the same executable in an AppContainer with the call worker's
   process mitigations (everything the call launcher sets except
   prohibit-dynamic-code and Win32k lockdown), a Job with a 512 MiB limit, and
   these capability sets, and repeats the capture there:

| Capability set (AppContainer) | Result |
| --- | --- |
| `internetClient`, `microphone`, `webcam` (the worker today) | `CreateForMonitor` failed, `0x80070005` |
| the three, plus `graphicsCaptureProgrammatic` | `CreateForMonitor` failed, `0x80070005` |
| the three, plus `graphicsCapture` | `CreateForMonitor` failed, `0x80070005` |
| the three, plus both | `CreateForMonitor` failed, `0x80070005` |

The control captured a frame (exit `0x0`). Each container run reached the
capture code (exit `0x5` is the stage `Item`, which is `CreateForMonitor`),
so the process, COM and the capture factory all ran inside the container.

The child reports through its exit code (the stage, then the two halves of the
`HRESULT`, in three runs per set). Nothing leaves the container otherwise.

## Command

```text
python tools/dev.py sandbox --offline
```

The wrapper builds the gate, runs the existing containment gate, then runs
`persephone-test-windows.exe --screen-probe`, and finally raises the gate's own
status. Raw output: `.local/screen-sandbox-run.txt` (not committed).

* `--screen-probe` exit status: **0** when run alone (the control captured,
  every container run finished). Through the wrapper it ran to the end and
  printed the same four results; the wrapper then raised the gate's status.
* The wrapper exit status: **1**. That is the existing containment gate, which
  ends with `probe error: one or more sandbox assertions failed` and
  `network_denial[appcontainer]: NOT_ESTABLISHED` on this machine. This change
  did not touch it, and a wrapper that fails there is unchanged from before:
  the screen probe now runs before the failure is raised so its result is not
  lost. The gate failure is not a pass and is not part of this report.

## Rerun from an identified commit

Run 19 November 2026, same machine, OS build, toolchain and command
(`python tools/dev.py sandbox --offline`), after review changes to the probe.

* Source: commit `d1f66b4ddff0f996d39786e790a912230e7638d3`, checked out
  with no local changes.
* `Cargo.lock` SHA-256:
  `2d598294b873b5bc5d4ec67acef8591061f1638d47f9326fb23f4933de37efb4`.
* The probe listed one 2560x1440 monitor (the first run listed two).
* Control: exit `0x0`, captured a frame.
* All four capability sets: exit `0x5`, `CreateForMonitor` failed,
  `HRESULT 0x80070005`. At this commit the probe joins the two halves only when
  both repeat runs fail at the stage of the first run, and labels the value so.
  All four sets met that condition.
* The screen probe ran to the end through the wrapper. It was not run alone
  this time.
* The wrapper exit status: **1**, from the same containment gate failure
  (`probe error: one or more sandbox assertions failed`,
  `network_denial[appcontainer]: NOT_ESTABLISHED`). At this commit the wrapper
  raises the gate's status before the probe's.

Raw output: `.local/screen-sandbox-run.txt` in that worktree (not committed;
the first run's raw output was no longer in that folder at the rerun).

## What this does and does not show

* Observed: on this machine, in this session, the four sets cannot create a
  capture item for a monitor. The same code captures outside the container.
* Not shown: that no other capability, an LPAC container, a packaged app with
  an identity, or another Windows build would work. Only the two candidates
  of the ADR were tried. `graphicsCapture` and `graphicsCaptureProgrammatic`
  are names the SID derivation accepts; the probe did not check that the
  system lists them as capabilities for an unpackaged process. The cause of
  `E_ACCESSDENIED` was not traced (a missing packaged identity is a likely
  one; this is inference).
* The launch in the probe repeats `LaunchSpec::call()` of
  `crates/media-broker/src/windows.rs`; the two can drift. The probe has no
  test that compares them.
* Capturing a window (instead of a monitor) was not tried.

## Consequence for the product

* `crates/call-worker/src/screen/windows.rs` keeps the capture code but its
  `CAPTURE_IN_CONTAINER` is `false`, so a screen start on Windows ends as
  `Unavailable` and no capture thread starts.
* The core does not show a Windows screen chooser; its Windows source
  reports unavailable. No capability was added to
  `crates/media-broker/src/windows.rs`.
* To enable Windows screen sharing later: find a capability set or identity
  that makes this probe capture a frame, add it to the call launcher and to the
  probe's sets, set `CAPTURE_IN_CONTAINER`, and add the chooser from the ADR.
  That needs a new decision if it relaxes any mitigation.

## Measurement of 25 November 2026: DXGI and GDI

Question: does any capture method other than Windows.Graphics.Capture run in
the call worker's AppContainer without a new capability or a weaker
mitigation?

* Date and machine: 25 November 2026, Windows 11 Pro x64 (build 26300),
  release build of the gate, rustc 1.99.1, one 2560x1440 monitor, one
  interactive session.
* Source: commit `fb911374ab6c7c8f83d9b081ef8a61c09e0adb05` on branch
  `claude/call-screen-share-all`, checked out with no local changes.
* `Cargo.lock` SHA-256:
  `cd6f78ba4e2b5e67d47009c6c1fef8e24e444e008ed808126d26ba9f13eec525`.
* Command: `python tools/dev.py sandbox --offline`. Raw output (not
  committed): `.local/screen-sandbox-run-2026-11-25-final.txt` in that
  worktree. Wrapper exit status: **1**, from the existing gate failure
  (`probe error: one or more sandbox assertions failed`,
  `network_denial[appcontainer]: NOT_ESTABLISHED`, also `lpac` and
  `restricted_token`). That failure is not part of this measurement and is not
  a pass. `--screen-probe` itself printed all results below and deleted its
  AppContainer profile.

### What the probe applies

The probe launches the container as the call launcher does
(`LaunchSpec::call()` in `crates/media-broker/src/windows.rs`): capabilities
`internetClient`, `microphone`, `webcam` plus the webcam device capability
SID, a Job (one process, 512 MiB, kill on close), and these process
mitigations: DEP, heap terminate, bottom-up and high-entropy ASLR, strict
handle checks, extension points disabled, font loading disabled, no remote
and no low-label images, system32 images first. The real call worker uses the
same set: `LaunchSpec::call()` turns off exactly two mitigations of the media
worker, prohibit-dynamic-code and Win32k system call disable
(`prohibit_dynamic_code: false`, `disable_win32k: false`; the audio stack
needs them), and the probe omits those two as well. So the probe has no
Win32k lockdown and no dynamic-code prohibition **because the real worker has
none**. The probe does not compare itself with the launcher in code (the two
can drift; a unit test in the media broker pins the launcher side).

### Results

| Method | Control (no container) | AppContainer, worker's three capabilities |
| --- | --- | --- |
| WGC `CreateForMonitor` | captured a frame | fails, `0x80070005` (`E_ACCESSDENIED`); same with `graphicsCaptureProgrammatic`, `graphicsCapture` and both |
| DXGI `DuplicateOutput` then `AcquireNextFrame` and CPU copy | `DuplicateOutput` worked; `AcquireNextFrame` timed out for 5 s, `0x887a0027` (`DXGI_ERROR_WAIT_TIMEOUT`): no frame, so the control did not capture | `DuplicateOutput` fails, `0x80070005` (`E_ACCESSDENIED`) |
| GDI `GetDC(NULL)` and `BitBlt` into a 32-bit DIB section (`CAPTUREBLT`) | captured a frame; not black, not uniform; mean colour `0x151515` | **captured a frame; not black, not uniform; mean colour `0x151515`** |

Notes:

* The pixel check counts pixels with a non-zero colour channel and pixels that
  differ from the first pixel. All-black and one-colour copies are failures
  (`BLACK`, `UNIFORM` in `screen_alt.rs`). The control and the container copy
  gave the same mean colour on the same desktop. That is consistent with a
  copy of the desktop, but it does not exclude a partial or wrong picture:
  two different pictures, or a partly zero-filled buffer, can have the same
  mean. The picture itself was not compared or looked at.
* The GDI run in the container is the worker's own code (`screen/gdi.rs`,
  compiled into the gate), on a thread set to per-monitor DPI awareness.
* The DXGI control did not capture on this static desktop: the first
  `AcquireNextFrame` returns only when the screen changes. The container run
  stopped earlier, at `DuplicateOutput`, so its result does not depend on the
  control. A DXGI frame in the container was never seen, and it is not shown
  that DXGI could not copy a frame if `DuplicateOutput` were allowed.
* One monitor only. Multi-monitor, other DPI scales, protected content, and
  a locked or secure desktop were not measured.

### Consequence

* GDI works in the container with no new capability and no relaxed
  mitigation. The worker's Windows backend is `screen/gdi.rs`;
  `CAPTURE_IN_CONTAINER` is `true`. The core gained a monitor chooser
  (`crates/core/src/platform/windows/screen.rs`).
* The same result means a process in this AppContainer can copy the screen
  with GDI whether or not a share is on. The worker has no Win32k lockdown.
  The share grant (one monitor, named by the core) does not stop a
  compromised worker from reading any monitor. ADR-0053 states this.
* GDI draws no capture border and no cursor.
* Not verified: a live call in which another member sees the share, and
  that ADR-0045's display affinity (main window excluded from capture) also
  hides the main window from a GDI copy.

## Message box cancel, 25 November 2026

ADR-0053 says that when the call ends while the core's screen question is
open, a thread timer closes the message box within about 100 ms. This
measures that path on a real desktop.

The test (`given_an_open_question_when_the_call_ends_then_it_closes` in
`crates/core/src/platform/windows/screen.rs`) opens a real box, sets the
cancel flag after 300 ms, and measures from the flag to the box's return. It
requires a cancel within 500 ms. It then opens a second box on the same
thread with the flag already set and requires that it stays open for at
least 50 ms, so a quit message left from the first box did not end it.

### First runs (not reproducible from one revision)

* Host: Windows 11 Pro, version 10.0.26300, two 2560x1440 monitors. The
  workstation was locked (`LogonUI.exe` running); the box still opened and
  closed.
* Code: commit 0eff4e2 with the uncommitted edits of the next commit on
  `claude/call-screen-share-all` (the test's bound, not `ask`, changed).
* Command, run three times:
  `cargo test --locked --offline -p persephone-core --lib -- --ignored open_question --nocapture`

Results: all three runs passed. The box closed 41.2 ms, 32.4 ms and 37.6 ms
after the flag was set; each run took 0.44 to 0.46 s in all.

Correction: this record gives no `Cargo.lock` hash or tool versions, and its
source was a commit plus uncommitted edits, so no single revision reproduces
it. The rerun below replaces it as the support for ADR-0053.

### Rerun at one revision

* Host: Windows 11 Pro x64, version 10.0.26300.9974, two 2560x1440
  monitors, one interactive session. The workstation was locked
  (`LogonUI.exe` running); the box still opened and closed.
* Source: commit `d271de051ef5e3a399a1eef24c50b485cbc1d7f2` on branch
  `claude/call-screen-share-all`, with no local changes before or after the
  runs (`git status --porcelain` empty).
* `Cargo.lock` SHA-256:
  `f90e3714eb4bc6476f8bbebfb7d59c8e60d4979b870ccd73dcc2b6e260346832`.
* Tools and build: rustc 1.99.1 (58cf8f9c3 2026-10-13), cargo 1.99.1
  (5f94df478 2026-08-27); the `test` profile (debug, unoptimized), with
  `CARGO_TARGET_DIR` set to `.local/build/rust` in the worktree.
* Command, run three times, each with exit status 0:
  `cargo test --locked --offline -p persephone-core --lib -- --ignored open_question --nocapture`
* Raw output (not committed): `.local/box-cancel-run-2026-11-25-d271de0.txt`
  in that worktree.

Results: all three runs passed. The box closed 51.1 ms, 28.9 ms and 45.0 ms
after the flag was set; each run took 0.44 to 0.47 s in all.

Limits: one machine and one DPI scale. The timer looks every 100 ms, so a
close can take up to about 100 ms plus the time the box needs to end; these
runs did not show the upper end. The case where `SetTimer` fails is a unit
test (no box opens), not a measurement.

## Rerun of the capture measurement, 25 November 2026

Question: does the result of the DXGI and GDI measurement above hold at the
current branch head, on two monitors, with the desktop unlocked?

A run at commit 5e98e72 on a locked desktop (`LogonUI.exe` running) was
inconclusive: the uncontained GDI control also copied an all-black screen,
so the container's copy could not be judged. It was not recorded here. This
rerun replaces it.

* Date and machine: 25 November 2026, 20:46 UTC. Windows 11 Pro x64, version
  10.0.26300.9974, two 2560x1440 monitors, one interactive session. The
  desktop was unlocked (`LogonUI.exe` not running) and in use.
* Source: commit `605676488bd3d60a64e2144c640569c21af0893b` on branch
  `claude/call-screen-share-all`, with no local changes before the run
  (`git status --porcelain` empty).
* `Cargo.lock` SHA-256:
  `f90e3714eb4bc6476f8bbebfb7d59c8e60d4979b870ccd73dcc2b6e260346832`.
* Tools and build: rustc 1.99.1 (58cf8f9c3 2026-10-13), cargo 1.99.1
  (5f94df478 2026-08-27); the release build that the wrapper makes.
* Command: `python tools/dev.py sandbox --offline`. Raw output (not
  committed): `.local/sandbox-run-2026-11-25-6056764.txt` in that worktree.
  Wrapper exit status: **1**, from the same existing gate failure as before
  (`probe error: one or more sandbox assertions failed`;
  `network_denial` is `NOT_ESTABLISHED` for `appcontainer`, `lpac` and
  `restricted_token`, and the restricted-token worker failed to start with
  `0xc0000142`). That failure is not part of this measurement and is not a
  pass. `--screen-probe` printed all results below and deleted its
  AppContainer profile.

| Method | Control (no container) | AppContainer, worker's three capabilities |
| --- | --- | --- |
| WGC `CreateForMonitor` | captured a frame | fails, `0x80070005` (`E_ACCESSDENIED`); same with `graphicsCaptureProgrammatic`, `graphicsCapture` and both |
| DXGI `DuplicateOutput` then `AcquireNextFrame` and CPU copy | captured a frame; mean colour `0x1b1f25` | `DuplicateOutput` fails, `0x80070005` (`E_ACCESSDENIED`) |
| GDI `GetDC(NULL)` and `BitBlt` into a 32-bit DIB section (`CAPTUREBLT`) | captured a frame; not black, not uniform; mean colour `0x232222` | **captured a frame; not black, not uniform; mean colour `0x232222`** |

Notes:

* The result matches the first measurement: only GDI copies a picture in
  the container, with no new capability and no relaxed mitigation.
* This time the DXGI control captured a frame, because the screen changed
  during the run. So the container's DXGI refusal at `DuplicateOutput` now
  stands next to a control that works.
* The probe listed both monitors but copies monitor 0 only
  (`Choice::First`). The second monitor was not copied in the container.
* As before, equal mean colours are consistent with a copy of the desktop,
  but they do not prove the same picture. The pictures were not compared or
  looked at.
* Not measured: a copy of the second monitor in the container, other DPI
  scales, protected content, a locked or secure desktop, and whether the
  Persephone main window is hidden from a GDI copy (ADR-0045).
