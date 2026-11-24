# Camera in the call worker's AppContainer (Windows)

Checked 24 November 2026 on Windows 11 Pro x64 (build 26300.9974), debug
build, rustc 1.99.1, one USB camera ("Web Camera", USB vendor 0BDA, product
5857). Source: branch `claude/linux-tray-bundle` at commit
`8ddada891ea0227616bfcea028f50df5a135fb5f` with the uncommitted changes of
this fix, `--locked --offline`. `Cargo.lock` SHA-256:
`1be647a6746274b040f1828135fa146048ff7c10937cdf952e110cb2756b3ff1`
(unchanged by the fix).

**Result: with the capabilities it had (`internetClient`, `microphone`,
`webcam`, all derived with `DeriveCapabilitySidsFromName`), the call worker
lists the camera but cannot open it: `IMFActivate::ActivateObject` fails with
`E_ACCESSDENIED` (`0x80070005`), and the camera reports `Unavailable`. With
the `webcam` device capability SID
`S-1-15-3-3845273463-1331427702-1186551195-1148109977` also in the token, the
same worker opens the camera and delivers pictures.**

Decision: [ADR-0048](../adr/0048-add-camera-video-to-calls-in-the-call-worker.md),
amendment of 24 November 2026.

## Trigger

The owner ran the camera test of ADR-0059 on 24 November 2026 at 14:14:25
local time. The microphone and speaker tests worked; the camera test did not.
Windows recorded the camera's last arrival at 14:14:00, so the camera was
connected during the test. The diagnostic log had no worker record, because
the camera backend reported every failure as `Unavailable` with no note.

## What was measured

The core's ignored test harness (`crates/core/src/platform/windows/call.rs`)
started the built worker through `ContainedCallHost`, which uses the product
launcher (`start_call_worker` in `crates/media-broker/src/windows.rs`): the
call AppContainer profile, the call mitigations and the Job. It sent
`SetDiagnostics { on: true }` and `TestDevice { Camera, None }`, and read the
worker's messages and the picture store's tile 0. A temporary build of the
worker also reported `CM_Get_Device_Interface_List_SizeW` sizes and the
Media Foundation device count from inside the container through its device
list. That temporary code is not part of the change.

| Run | Camera interfaces (PnP) | Media Foundation cameras | Camera test |
| --- | --- | --- | --- |
| Control: this user's process, no container (PowerShell) | `KSCATEGORY_VIDEO_CAMERA` and `KSCATEGORY_VIDEO` listed | not measured | not run |
| Container, the shipped capabilities | listed | 1 | note `camera ActivateObject failed: HRESULT 0x80070005`, `Camera { Unavailable }`, `Left`, no picture |
| Container, plus the `webcam` device capability SID | listed | 1 | `Connected`, `Camera { On }`, a 640x360 picture on tile 0, `VideoStopped`, `Camera { Off }`, `Left` |

An earlier run in the same session, while the camera was unplugged, listed no
camera in or out of the container. It is no evidence about the container.

## Repeatable check

```text
cargo test --locked -p persephone-core --lib platform::windows::call -- --ignored
```

After `python tools/dev.py build`, with a camera connected. Run on 24 November
2026 with the fix: both ignored tests passed
(`given_the_built_worker_when_started_then_it_is_ready_and_contained` and
`given_a_camera_when_tested_in_the_container_then_a_picture_arrives`), exit
status 0. The camera test takes the camera for about one second.

## What this does and does not show

* Observed: on this machine, the call worker's camera open needs the device
  capability SID. The SID that `DeriveCapabilitySidsFromName("webcam")`
  derives does not grant it. The same worker and launcher open the camera
  when both SIDs are present.
* Not shown: which object's access check refuses the named SID (the camera
  device, the Camera Frame Server or another broker). This is inference: a
  packaged app's token carries device capabilities as `S-1-15-3-<GUID>`, so
  the camera stack checks that form.
* Not shown: other Windows builds, other cameras, or a call with video to
  another client. The camera test opens the camera the way a call does
  (ADR-0059), but no call was run.
* The added SID grants the `webcam` device capability, which the worker was
  meant to have since ADR-0048. No other capability or mitigation changed.
* The screen capture probe (`--screen-probe`) now launches with the same SID,
  to stay in step with the call launcher. It was not rerun;
  [screen-share-windows.md](screen-share-windows.md) still describes runs
  without it.
