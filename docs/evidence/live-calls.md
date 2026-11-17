# Audio calls with Element Call

Checked 16 and 17 November 2026 on Windows 11 Pro x64 (build 26300), with
debug builds of Persephone (Rust 1.99.1, Matrix Rust SDK 0.19.1, LiveKit Rust
SDK 0.11.0, libwebrtc `webrtc-a545de4`) against a disposable Synapse 1.163.0,
livekit-server 1.13.11 and lk-jwt-service 0.10.0. The other client was
Element Call 0.28.2 (standalone) in the owner's own Chromium-based browser.
The source was commit `f0cfab7` plus the uncommitted call-worker, core and UI
changes of this branch. This is the live run that ADR-0043 requires before
any claim that calls work.

**Result: Persephone and Element Call heard each other in an end-to-end
encrypted MatrixRTC call. Mute, key rotation on leave and rejoin, an explicit
leave, lock, closing the app, a killed worker, the proxy warning (accept and
decline) and the missing loopback exemption each behaved as ADR-0043
describes. The run found three product faults that blocked every call; all
three are fixed and the fixes were part of the passing runs.**

## Inputs

| Item | Value |
| --- | --- |
| Topology, downloads, Synapse settings | [Live call test plan](../planning/live-call-test.md): topology, spike and setup results |
| Persephone | `persephone` with `--features dev-disposable-servers`, `persephone-ui`, `persephone-call-worker` with `--features dev-disposable-servers` and `-C target-feature=+crt-static`, built `--locked --offline` (debug) at 22:51 to 22:54 on 16 November |
| Accounts | Harness accounts `p0_alice` (Persephone) and `p0_bob` (Element Call; cross-signed by the SDK peer `p4_peer`, which also created the encrypted room) |
| Networking | livekit-server in WSL with `node_ip` set to the WSL address; the call worker had a dev-only `privateNetworkClientServer` capability and an owner-added loopback exemption for its AppContainer (removed by the owner before steps 19 to 21) |
| Microphone and speaker | Razer Kraken V4 X, chosen by the owner in Settings, Calls |

## Method

The owner joined, spoke and listened, and reported what was heard. The agent
read, for every step:

- the room state (`org.matrix.msc3401.call.member` events) from Synapse's
  database, read-only;
- the Synapse access log, by request path and time, with tokens and delay IDs
  removed;
- the call worker's process ID, lifetime and exit code (a PowerShell
  watcher);
- the page's call state through the bridge (`call_status`, `call_devices`,
  `proxy_setting`) over WebView2's loopback DevTools port;
- in steps 20 and 21, the CONNECT log of a loopback SOCKS5 relay
  (`.local/live-call/scripts/socks5.py`).

A step passed only when these records showed the path under test. Audio
that stopped for another reason did not count (see "Attempts not counted").
The step numbers below are those of the run log in the
[test plan](../planning/live-call-test.md#run-log-16-november-2026-first-attempt).

## Results

| Plan step | What was checked | Result |
| --- | --- | --- |
| 3 Readiness | The call button is available in the encrypted room | Pass |
| 4 to 6 Join and audio | One LiveKit room; Element Call hears Persephone and Persephone hears Element Call | Pass after the three fixes below (log step 11) |
| Devices | Settings lists microphones and speakers through a contained worker that exits after about 1 s with code 0; the saved choice survives a restart | Pass (log steps 11, 13) |
| 7 Mute | Element Call goes silent, then hears Persephone again | Pass (log step 13) |
| 9 Rejoin | Element Call leaves and rejoins; Persephone sends its key 0.8 s after the rejoin; audio both ways | Pass (log step 14) |
| 10 Leave | Persephone leaves twice; each leave is a delayed-event `send`, so `{}` at once; each rejoin hears both ways | Pass (log step 14) |
| Close | Persephone closed during a call; no leave sent; Synapse ends the membership 7.9 to 8.0 s after the last restart | Pass, three times (log steps 12, 14, 17) |
| 11 Lock | The worker ends at once and the audio stops; no leave request; no new sync round; `{}` 8.0 s after the last restart | Pass (log step 16) |
| 12 Worker killed | Worker ends with exit code 1 from Task Manager; Persephone stays open, shows "The call process stopped."; leave sent about 0.2 s after the kill | Pass (log step 18) |
| 13 Negative control | Without the loopback exemption the worker cannot reach `ws://127.0.0.1:7880`; the call ends as `connection_failed` after 33 s; Persephone sends the leave | Pass (log steps 19, 20) |
| 14 Proxy, accept | The core's own window warns before the join; after "join anyway", Matrix and `/sfu/get` traffic goes through the SOCKS5 relay and only the worker connects to the SFU directly | Pass (log steps 19, 20) |
| 14 Proxy, decline | No Matrix call request, no `/sfu/get`, no new relay connection, no worker | Pass (log step 21) |
| 8 Delayed leave over 60 s | Restarts every 5 s throughout calls of several minutes | Pass (seen in every log above) |

### Product faults found and fixed

1. **No IO driver in the worker's runtime.** The worker panicked (exit code
   101) at its first LiveKit socket. Every call would have failed. Fixed in
   `crates/call-worker/src/main.rs`, with a test that opens a socket.
2. **The sender's key index was never set.** Native libwebrtc encrypts with
   the frame cryptor's own index, which stays at 0 until set; LiveKit's Rust
   SDK does not set it. Every frame went out under Persephone's first key,
   so later joiners heard nothing and a rotation did not change the key in
   use. **Security effect:** a member who left could still decrypt
   Persephone's later audio. Element Call's log confirmed the cause. Fixed in
   `crates/call-worker/src/session.rs` (`use_own_key_index`), and ADR-0043
   now states both steps of using a key.
3. **The membership changed the SFU URL.** The core wrote the advertised
   `http://127.0.0.1:6080` back as `http://127.0.0.1:6080/` (`Url::as_str`).
   Element Call compares transports as strings, opened a second connection
   for the slashed URL, and never attached Persephone's audio. Fixed in
   `crates/core/src/calls/readiness.rs` (`SfuService` keeps the advertised
   string) and `membership.rs`, with tests for both forms.

### Findings

- While the core's proxy warning is open, the page's call button is disabled
  with no text, so the wait for an answer looks like a fault.
- The run log of the [test plan](../planning/live-call-test.md) lists the
  earlier open findings: the delayed leave is sent instead of cancelled when
  the membership is refused, readiness does not warn about power levels, a
  worker panic shows no detail, the STUN server is not named in the proxy
  warning, and `internetClient` excludes LAN SFUs.

### Attempts not counted

- Lock, first attempt: the call had ended by an explicit leave before the
  lock (the sync loop started new rounds after the leave). Repeated.
- Worker kill, first attempt: the owner ended the whole Persephone group, so
  this was another close. Repeated with only the worker.
- A second "join anyway" at 23:39:52 ended with an accidental close; the
  owner's answer to its warning was not confirmed.

## Checks run on the fixed source

Run from the repository root on 16 November 2026, after the third fix:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Exit 0 |
| `cargo test --locked --offline --no-fail-fast -p persephone-core -p persephone-call-protocol` | Exit 0; core 371 unit tests (2 ignored), 13 binary tests and every integration test passed; protocol 19 |
| `cargo test --locked --offline -p persephone-call-worker`, with and without `--features dev-disposable-servers` (`LK_CUSTOM_WEBRTC` set, `+crt-static`) | 19 passed each |
| `cargo clippy --locked --offline -p persephone-core --all-targets -- -D warnings` | Exit 0 |

The device-selection change (core, IPC, UI) was also checked by its own
`persephone-core-protocol`, `persephone-contracts`, `persephone-desktop`,
`tools/dev.py contracts` and `tools/dev.py frontend` runs, all passing. The
first core and worker attempts failed for build-environment reasons (the
media-worker test needs that worker built in the same target directory; the
call worker links only with `+crt-static`); the reruns passed.
`python tools/dev.py check` was not run.

## Limits

- One run, one host, Windows only. Linux and macOS refuse calls by design.
- Synapse only. Merovingian was not probed for delayed events or transport
  discovery.
- Plain `ws://` and `http://` on loopback with the dev feature. The vendored
  `livekit-net` TLS 1.3 path and the 256-key ring patch past index 255 were
  not exercised. A TLS deployment is needed.
- Media went to the WSL address under a dev-only
  `privateNetworkClientServer` capability, which release builds do not have.
  Which ICE candidate pair (UDP or ICE-TCP) carried it was not recorded.
  TURN relaying was not exercised.
- Two participants only. Group calls, a member on another SFU, and a member
  whose device is not cross-signed were not run.
- Element Call 0.28.2 in compatibility mode only; not Element X, and not
  Element Call's `m.rtc.member` mode.
- Audio quality was judged by ear. Whether audio continued without a gap
  while Settings listed devices during a call was not checked.
- The owner's report and the server records are the evidence; no packet
  capture was made. The negative control's cause is inferred from one
  changed condition.
- The debug builds carried dev-only features and the fixes were not yet
  committed. A release build was not run live.
