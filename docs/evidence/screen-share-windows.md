# Screen capture in the call worker's AppContainer (Windows)

Checked 19 November 2026 on Windows 11 Pro x64 (build 26300), release build of
the containment gate, rustc 1.99.1, two 2560x1440 monitors, one interactive
session. Source: branch `claude/call-screen-share`, worktree
`.local/wt-call-screen`, `--locked --offline`. `Cargo.lock` gained one line
(the gate package now depends on the already locked `windows 0.62.2`). This
first run did not record its source revision or lockfile hash. A later run
from an identified commit gave the same results; see
[Rerun from an identified commit](#rerun-from-an-identified-commit).
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
