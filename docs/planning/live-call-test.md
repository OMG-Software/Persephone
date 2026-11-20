# Live call test plan

Written 16 November 2026. Status: **run on 16 and 17 November 2026 on
Synapse**; results in [Audio calls with Element Call](../evidence/live-calls.md).
The Merovingian run and a TLS deployment are still open. This plan is the
gate before any claim that Persephone calls work (ADR-0043). A build, unit
tests or the contained-worker `Ready` test do not prove a call.

## Goal

Persephone (Windows, contained call worker) and Element Call web (Chromium)
join one MatrixRTC call in one encrypted room on a disposable Synapse. Each
side hears the other. The run records membership, the delayed leave, frame
keys, mute, leave, lock and worker failure.

Merovingian follows in a second run, after a probe shows that it serves
delayed events and RTC transport discovery.

## Topology

| Part | Where | Address |
| --- | --- | --- |
| Synapse 1.163.0 (the version of the October Element Web run) | WSL Ubuntu 24.04, Python venv | `http://127.0.0.1:8008` through WSL localhost forwarding (TCP) |
| livekit-server v1.13.11 | Windows or WSL: see the networking choice | `ws://127.0.0.1:7880`, ICE-TCP `7881`, UDP `50100-50200`, `node_ip: 127.0.0.1` |
| lk-jwt-service v0.10.0 | WSL, Linux binary (decided by the spike) | `http://127.0.0.1:6080` |
| Element Call v0.28.2 (standalone) | Static server on `127.0.0.1` only, Claude built-in browser | `http://localhost:<port>` (a secure context) |
| Persephone | Windows, debug, `dev-disposable-servers` | core, UI host, contained call worker |

Reasons:

- livekit-server runs on Windows because WSL NAT mode forwards only TCP
  through `localhost` (the WSL page says nothing about UDP). Media on
  Windows loopback avoids the question.
- Element Call's own dev stack (`docker-compose-dev.yml`) needs a checked-in
  local CA. Its private key is public, so trusting that CA in Windows would
  let anyone mint trusted certificates. **Do not trust it.** This plan uses
  loopback plain HTTP and WebSocket instead, which the dev feature already
  allows to `127.0.0.1` only.
- Synapse needs no Docker. The October interop run used a pip Synapse in WSL.

## Required code change before the run (done 16 November 2026)

The worker refuses `ws://127.0.0.1` today. The core's `dev-disposable-servers`
feature does not reach `persephone-call-protocol`, and the worker crate has
no such feature. This fails closed, which is correct, but it blocks a
loopback test.

1. `crates/core/Cargo.toml`:
   `dev-disposable-servers = ["persephone-call-protocol/dev-disposable-servers"]`.
2. `crates/call-worker/Cargo.toml`: a `dev-disposable-servers` feature with
   the same forward.
3. `tools/dev.py`: no default change. The live run builds with the feature
   by hand. Release builds stay without it.
4. A test that shows a release build (no feature) refuses `ws://127.0.0.1`
   in both the core and the protocol. Both tests already exist
   (`calls/sfu.rs` and `call-protocol`).

Checked: `cargo test --locked --offline -p persephone-call-protocol` passed
without the feature (16 tests) and with it (17). `cargo test --locked
--offline -p persephone-core --features dev-disposable-servers calls::`
passed (97 tests). The worker has not yet been built with the feature.

Limit: plain `ws://` does not exercise the vendored `livekit-net` TLS 1.3
patch. A later run against a TLS test deployment must cover it.

## Interop risks (go/no-go checks in the spike)

| # | Risk | Check |
| --- | --- | --- |
| R1 | Element Call 0.28 may send the newer `m.rtc.member` sticky events (MSC4354) and not the legacy `org.matrix.msc3401.call.member` state events. Then neither client sees the other as a member. | Read the 0.28.2 config options. Join with Element Call alone and read the room state. Use a config that sends legacy events. If none exists, stop and report to the owner. |
| R2 | Persephone calls lk-jwt `POST /sfu/get` (deprecated). Element Call may call `POST /get_token`. The two may map to different LiveKit room names. | Join both and list rooms on livekit-server. One room with two participants is a pass. |
| R3 | lk-jwt checks the OpenID token through the homeserver's federation `openid/userinfo`. It is not known whether it can use plain HTTP to `127.0.0.1:8008`. | Spike with a token from `p4_peer` or curl. Fallback: a Synapse TLS federation listener with a throwaway self-signed certificate, and lk-jwt `LIVEKIT_INSECURE_SKIP_VERIFY_TLS`. That bypass is in the third-party fixture only, never in Persephone. |
| R4 | lk-jwt 0.10 may need an application-service registration even for `/sfu/get`. | Spike. Register one only if the spike needs it. |
| R5 | Synapse 1.163 needs the right flags for delayed events and transports. The exact delayed-events flag name is not confirmed. | Read the 1.163.0 config docs. Check `/versions` `unstable_features` and the transports endpoint before the run. |
| R6 | Persephone parses only the legacy `livekit` transport with `livekit_service_url`. Synapse deprecated it in 1.161 in favour of `m.livekit` with `url`. | Configure the legacy form for this run. Record the newer form as a compatibility follow-up. |
| R7 | Frame key format and HKDF ratchet must match Element Call. | Audio both ways with encryption on is the check. Silence or decode errors on one side is a fail. |

## Spike results (16 November 2026)

Read from the verified Element Call 0.28.2 bundle (its 159 per-file hashes
match the release `.sha256` file) and from lk-jwt-service source at tag
`v0.10.0`. No server ran yet. These are readings of code, not observed
behaviour.

| # | Finding | Status |
| --- | --- | --- |
| R1 | Element Call's `matrix-rtc-mode` setting defaults to `compatibility`. A deployment `matrix_rtc_mode` config can override it. Its membership content uses `focus_active: {type: livekit, focus_selection: multi_sfu}`, which is what Persephone sends. Its default delayed leave is 18 s, restarted every 4 s. | Likely pass. Confirm in the run. |
| R2 | Element Call calls `POST {service}/sfu/get` with `{room, openid_token, device_id, ...}`, as Persephone does. lk-jwt names the LiveKit room `sha256(room, slot)` and gives `/sfu/get` callers the identity `{user}:{device}`, as Persephone expects. | Likely pass. Confirm in the run. |
| R3 | lk-jwt resolves the token's `matrix_server_name` as a federation server and calls `{base}/_matrix/federation/v1/openid/userinfo`. Plain HTTP is not a supported path. | Use the fallback: a TLS federation listener on Synapse. |
| R4 | `LIVEKIT_AS_REGISTRATION_FILE` is optional. `LIVEKIT_FULL_ACCESS_HOMESERVERS` must not be empty. | No application service needed. |
| R5 | Not checked: no Synapse is installed. The October venv was in a checkout that no longer exists. | Open. Needs the PyPI download. |
| New | lk-jwt calls the LiveKit server API (CreateRoom, GetParticipant) at `LIVEKIT_URL`. The same URL goes to clients. lk-jwt in WSL NAT mode cannot reach `127.0.0.1` on Windows. | Open. See the networking choice. |

Changed by the spike:

- lk-jwt runs in WSL (the Linux binary, already verified).
- Synapse uses `server_name: localhost:8448`, with a second listener on
  `127.0.0.1:8448` (TLS, resources `federation` and `openid`, a throwaway
  self-signed certificate made with `openssl` in `.local/`). lk-jwt sets
  `LIVEKIT_INSECURE_SKIP_VERIFY_TLS`. Persephone and Element Call use only
  the plain client listener on `8008`. The bypass lives in the third-party
  fixture, never in Persephone.

### Networking choice (open)

| Option | Owner action | Effect |
| --- | --- | --- |
| A (recommended) | Approve the `livekit_1.13.11_linux_amd64.tar.gz` download (19,464,037 bytes, SHA-256 `1e9ee90ca301e1842b2887fc77cb122759756a9c8aeb4334ee91de3325ec43fb`) | All servers in WSL on loopback. Clients reach them through WSL localhost forwarding, which is TCP. Media must use LiveKit ICE-TCP on `7881`. UDP media is not tested. |
| B | Set `networkingMode=mirrored` in `.wslconfig` and restart WSL | Windows and WSL share `127.0.0.1`. livekit-server stays on Windows. UDP media is tested. Changes WSL networking for every distribution until reverted. |
| C | Allow inbound TCP 7880 to `livekit-server.exe` on the WSL virtual interface in Windows Firewall | livekit-server stays on Windows. A small forwarder in WSL maps `127.0.0.1:7880` to the Windows host. UDP media is tested. |

## Setup results (16 November 2026, option A)

The owner chose option A and approved the Synapse and LiveKit Linux
downloads.

- `livekit_1.13.11_linux_amd64.tar.gz` matched its size and SHA-256.
- Synapse 1.163.0 was installed from PyPI into a WSL venv under
  `.local/live-call/` (59 packages; the list is in the pip report there).
- R5 resolved from the installed code: `max_event_delay_duration` turns on
  delayed events (no separate flag), and `experimental_features.msc4143_enabled`
  turns on the unstable transports path. `/versions` then lists
  `org.matrix.msc4140` and `org.matrix.msc4143`.
- The launcher (`.local/live-call/scripts/call_stack.py`, not committed)
  starts Synapse, livekit-server and lk-jwt on `127.0.0.1` and registers
  `p0_alice` and `p0_bob`.
- Smoke test from WSL and from Windows: login, transport discovery
  (`livekit_service_url: http://127.0.0.1:6080`), OpenID token, then
  `POST /sfu/get` returned `ws://127.0.0.1:7880` and a JWT. The JWT subject
  was `@p0_alice:localhost:8448:SMOKE`, the format Persephone expects. lk-jwt
  checked the token over the TLS federation listener and created the LiveKit
  room. **This settles R3 and R4 by observation.**
- WSL forwards LiveKit's dual-stack ICE-TCP listener to Windows on
  `[::1]:7881` only, but LiveKit advertises `127.0.0.1:7881`. A test-only
  Windows relay (`tcp_relay.py`, loopback only) maps `127.0.0.1:7881` to
  `[::1]:7881`.
- Element Call is served from `127.0.0.1:8090` with a config naming only the
  disposable homeserver. On first load it requested only `127.0.0.1`
  addresses.
- Debug builds with `dev-disposable-servers`: `persephone`, the UI host, the
  media worker and the call worker all built.

### Found during setup

- **R8: cross-signing of the Element Call device.** Persephone sends frame
  keys only to devices that their owner has cross-signed, and accepts keys
  only from them. Element Call standalone does not verify devices itself.
  The run uses `p4_peer` as Bob: `bootstrap` sets up Bob's cross-signing, then
  `sign-devices` signs Bob's Element Call device with Bob's self-signing key.
- **Element Call sign-in.** Its password is a generated test secret in the
  harness profile. Element Call is a third-party app, so the owner types it
  in the browser pane; the agent does not.

## Run log (16 November 2026, first attempt)

Observed, in order:

1. Bob (`p4_peer`) bootstrapped cross-signing and created the encrypted
   room. Persephone signed in as Alice, accepted, set up cross-signing and
   recovery. **Step 3 (readiness): pass.**
2. First join: Synapse refused the membership event (403). `p4_peer` rooms
   need power level 50 for unlisted state events. Persephone showed "Your
   homeserver refused the call membership." Bob set
   `org.matrix.msc3401.call.member` to 0.
3. Join without the loopback exemption: membership, delayed leave, OpenID
   and JWT all succeeded; LiveKit saw no connection; the call ended and the
   membership was removed. **Step 13 (negative control): pass.** The UI
   reason was `worker_stopped`, not a network reason (see 5).
4. The owner's `-n=` exemption did not register; the `-p=<SID>` form did.
5. With the exemption, the worker exited with code 101 (a Rust panic) 0.31 s
   after `Join`. Cause: its tokio runtime had no IO driver, so the first
   LiveKit socket panicked. **Product bug, fixed** in
   `crates/call-worker/src/main.rs` (`build_runtime` enables IO; tokio
   `net` feature declared; a new test opens a loopback socket on it).
   Every call would have failed this way.
6. After the fix the worker reached LiveKit (RTC session started), but ICE
   failed: LiveKit advertised only `127.0.0.1` candidates, and native
   libwebrtc ignores loopback adapters, so it offered only LAN, Hyper-V,
   WSL-host and server-reflexive candidates. No check reached LiveKit. The
   worker gave up after 60 s with `Failed(Network)`; the UI showed
   `connection_failed`. A Chromium guest session earlier also failed ICE on
   this topology.
7. UDP from Windows to the WSL VM address (`172.31.110.46`) works.
8. With a dev-only `privateNetworkClientServer` capability and LiveKit
   `node_ip` set to the WSL address, ICE connected. Element Call (a second
   browser) and Persephone joined one LiveKit room. Audio from Element Call
   was heard in Persephone. Audio from Persephone was silent in Element
   Call, and Element Call never showed Persephone as speaking.
9. Diagnosis of the silent direction:
   - Windows showed the worker's capture session on the default microphone
     for all three roles (Razer Kraken V4 X). Its session level meter
     reached 0.093 while the owner spoke; Opera's session on the same
     device reached 0.097. Microphone privacy was `Allow`.
   - The SFU received 22,903 packets from Persephone's track in 37 minutes,
     about 95 bytes each, at about 10 packets per second. That is Opus
     speech mixed with DTX silence, so capture and encoding worked.
   - Persephone sent its key to Bob's device at each of Bob's joins. The
     content matches matrix-js-sdk's `ToDeviceKeyTransport` field for field.
   - Cause: native libwebrtc encrypts with the frame cryptor's own key
     index, which stays at 0 until `SetKeyIndex` is called.
     `KeyProvider::set_key` stores a key but does not change that index,
     and the LiveKit Rust SDK never calls `set_key_index`. Every frame went
     out under Persephone's first key (index 0). Bob joined later, so Bob
     received only rotated keys (index 1 and up) and could not decrypt.
     livekit-client in the browser switches the index on `setKey`, which is
     why the other direction worked. **Product bug, fixed** in
     `crates/call-worker/src/session.rs` (`use_own_key_index` after publish
     and after each stored own key). Not yet retested live.
   - Security effect before the fix: a rotation on leave did not change the
     key that encrypted Persephone's media. A member present when
     Persephone joined would have kept the key to all of Persephone's
     later audio, even after leaving.
10. Retest with the key-index fix, Element Call left and rejoined: still
    silent. Element Call's console showed that it now held Persephone's
    key at index 0 and subscribed to the track, but a second fault stopped
    playback:
    - Persephone's membership advertised `livekit_service_url`
      `http://127.0.0.1:6080/`. Bob's membership and the homeserver's
      `rtc/transports` answer both use `http://127.0.0.1:6080`, without
      the slash. The core parsed the advertised string into a `Url` and
      wrote `Url::as_str()`, which adds `/` to an empty path.
    - Element Call groups members by transport and compares the service
      URL as a string. It opened a second connection for Persephone's
      "different" transport, asked `http://127.0.0.1:6080//get_token`
      (blocked by CORS), and logged Persephone's participant as missing on
      that connection. It never attached the audio that the first
      connection received.
    - **Product bug.** The membership must carry the service URL exactly as
      the homeserver advertised it. Fixed: discovery keeps the advertised
      string (`SfuService` in `crates/core/src/calls/readiness.rs`), and the
      membership writes it unchanged.
11. Retest with both fixes and the new device choice (debug build with
    `dev-disposable-servers`, 22:56):
    - Settings listed 3 microphones and 3 speakers through a contained
      listing worker, which then exited. The owner chose the Razer Kraken
      V4 X for both.
    - Persephone's membership carried `http://127.0.0.1:6080`, the same as
      Bob's.
    - The owner was heard in Element Call. **Audio worked in both
      directions** for the first time.
    - A second device listing during the call started and ended its own
      worker, and the call worker kept running. Whether audio continued
      without a gap at that moment was not checked.
12. Persephone closed during the call (exit code 0, about 23:00). It sent
    no leave. Its last delayed-leave restart was at 23:00:03.086, and
    Synapse ended the membership (`{}`) at 23:00:11.023, 7.9 s later. This
    matches the exit path in ADR-0043: the worker is killed and the delayed
    leave ends the membership within about 8 s.
13. Persephone restarted and rejoined at 23:01:43 (new worker, same saved
    devices, membership URL without the slash). Run step 7: the owner
    muted and unmuted in Persephone, and Element Call went silent and then
    heard the owner again. The owner reported "as expected".
14. Run steps 9 and 10. The owner reported two-way audio after every
    rejoin: Element Call's and both of Persephone's. Server record
    (Synapse access log and state events):
    - Element Call left at 23:02:27.6 and rejoined at 23:02:33.2.
      Persephone sent no key while Bob was absent and sent one encrypted
      to-device message at 23:02:34.0, 0.8 s after Bob's rejoin.
    - Persephone left at 23:03:09 and 23:03:18. Each leave was a delayed
      event `send` (`{}` at once), not a new state write. Each rejoin
      created a new delayed leave, wrote the membership, got an OpenID
      token and sent its key to Bob within 2 s.
    - Persephone then closed (exit code 0). Its last restart was at
      23:03:34.3 and the delayed leave ended the membership at 23:03:42.3,
      8.0 s later.
15. Run step 11, first attempt: **inconclusive.** The owner reported
    "locked, audio stopped". But the server shows that the call ended with a
    delayed-event `send` (an explicit leave) at 23:06:39.55, 2.8 s after a
    restart, and alice's sync kept starting new rounds at 23:06:40.7 and
    23:06:45.7. A lock closes admission first, so no sync round can start
    after it. The lock therefore came after the call had already ended,
    and the audio stopped because of the leave. `Call::finish` sends no
    leave when the session ended, and that path was not exercised. Repeat
    with Lock as the only action.
16. Run step 11, repeated with Lock as the only action: **pass.**
    - Persephone joined at 23:15:47. The owner spoke, then chose Lock
      Persephone and left the app locked. The audio stopped.
    - After the lock no `persephone-call-worker` process was left. The core
      and UI processes stayed alive.
    - Alice's last delayed-leave restart started at 23:16:02.468. The next
      one, due near 23:16:07.5, never came. Alice sent no delayed-event
      `send` and no `{}` state write. The sync round that was open at the
      lock finished its 5 s long poll at 23:16:07.449, and no new round
      started.
    - Synapse ended the membership (`{}`) at 23:16:10.468, 8.0 s after the
      last restart.
17. Run step 12, first attempt: **not the intended test.** The owner ended
    the whole Persephone process group in Task Manager, not only the call
    worker. Persephone joined at 23:18:28 and exited at 23:18:46 with code
    0. It sent no leave. Its last restart was at 23:18:43.3, and the
    delayed leave ended the membership at 23:18:51.264, 8.0 s later. This
    is a third pass of the exit path (steps 12 and 14), not a worker kill.
    Repeat by ending only `persephone-call-worker.exe` on the Details tab.
18. Run step 12, repeated by ending only `persephone-call-worker.exe` on the
    Details tab: **pass.**
    - The watcher saw worker pid 63648 end with exit code 1 (the
      `TerminateProcess` code) after 26.8 s, near 23:30:26. The core and UI
      processes kept running.
    - Alice's delayed-event `send` started at 23:30:26.08, about 0.1 to
      0.2 s after the kill. Synapse wrote `{}` at 23:30:29.302, 4.2 s after
      the last restart, so the leave came from Persephone and not from the
      8 s timer. (Synapse was slow then: this request took 3.8 s, and the
      join's membership write took 7.4 s.)
    - `call_status` reported phase `ended` with `worker_stopped`. The call
      bar showed "Call ended" and "The call process stopped." with a
      Dismiss button.
19. Run steps 13 and 14 together, after the owner removed the loopback
    exemption. The profile proxy was set to `127.0.0.1:1080` (nothing
    listening; the running client keeps its direct connection until a
    restart).
    - Join: the core's own window warned that the call bypasses the SOCKS
      proxy. While it was open, the page's call button stayed disabled with
      no text. The owner chose to join anyway. **Accept path: pass.**
    - After the answer, near 23:38:50, the join ran in order: TURN
      credentials, delayed leave, membership, OpenID token, key to Bob.
    - The worker (pid 48556) could not reach the SFU at
      `ws://127.0.0.1:7880`. After 33.2 s the call ended as
      `connection_failed` ("The connection to the call server failed."),
      and the worker exited with code 1. Persephone sent the leave
      (delayed-event `send` from 23:39:23.0, `{}` at 23:39:24.977).
      **Negative control: pass.** The only change from the working runs
      was the removed exemption, so the AppContainer is what blocked
      loopback. This is an inference from one changed condition, not a
      packet capture.
    - The decline path was not tried.
    - Finding: while the warning is open, the page gives no hint that an
      answer is awaited in another window.
    - A second join at 23:39:52 also went ahead (membership, OpenID, key
      send); the owner then closed Persephone by accident at 23:40:25.
      Which answer the owner gave on that join was not confirmed.
20. Same conditions with a working proxy: a loopback SOCKS5 relay
    (`scripts/socks5.py`, which logs each CONNECT) on `127.0.0.1:1080`, and
    Persephone restarted so that its client uses it.
    - The relay logged Persephone's connections to Synapse
      (`127.0.0.1:8008`) and one to the token service (`127.0.0.1:6080`,
      the core's `/sfu/get`). It logged no connection to the SFU
      (`127.0.0.1:7880`): only the call worker connects there, directly,
      after the warning.
    - The owner accepted the warning. The call had no audio and ended as
      `connection_failed`, as in step 19: the exemption was still removed.
21. Run step 14, decline path: **pass.** The owner joined again and
    declined the warning (reported 23:45). From 23:44:40 to 23:45:50 alice
    sent no call request to Synapse (no TURN request, delayed leave,
    membership, OpenID token or to-device key). The SOCKS5 relay logged no
    new connection, so no `/sfu/get` was sent. No call worker process
    started. The call state still showed the previous ended call.

Findings to fix or decide later. Status as of 19 November 2026:

- When the membership event is refused, the cleanup sends the delayed leave
  instead of cancelling it.
  *Fixed 19 November 2026 in `crates/core/src/calls/controller.rs`.* The
  flag `membership_sent` was set before the send and never cleared. It is
  now cleared when the homeserver answers with a client error (4xx), so the
  cleanup cancels the delayed leave. A timeout, a lost connection or a 5xx
  may hide a write, so those still send the leave. Test:
  `given_membership_failures_then_only_a_client_error_is_a_refusal`.
- Readiness does not warn when power levels block call membership.
  *Fixed 19 November 2026 in `crates/core/src/calls/status.rs`.* A new code
  `not_permitted` follows `not_encrypted`. Tests: `given_power_levels_then_member_state_permission_follows_the_room`,
  `given_no_power_levels_event_then_only_the_creator_may_send`.
- A worker that panics shows "The call process stopped." The panic message
  is lost because the worker has no stderr by design.
  *Fixed 19 November 2026 without a message.* The core reads the exit status
  of an ended worker (`crates/media-broker`, `crates/core/src/calls/worker.rs`
  `exit_is_crash`, `controller.rs` `stop_reason`). A panic (exit 101), the
  worker's own unreportable exit, a Windows crash status or a crash signal
  ends the call with the new code `worker_crashed`. The Unix part of the
  status read was not compiled or run on the development machine.
- The worker gathered a server-reflexive candidate, so it contacted a STUN
  server that the SFU named. The proxy warning names only the SFU and the
  TURN server. Check which STUN server LiveKit sends by default.
  *Fixed 19 November 2026 (text only) in `crates/core/src/platform/call_host.rs`.*
  The warning now says that STUN or TURN servers named by the call server
  also see the IP address. ICE behaviour is unchanged. The worker passes the
  homeserver's TURN servers, and the core keeps only `turn:` and `turns:`
  URIs. `livekit` 0.11 uses the SFU's `ice_servers` from the join response
  only when the worker's list is empty, so the STUN server comes from the
  SFU when the homeserver has no TURN. A UDP TURN server also answers STUN
  binding requests, so with a homeserver TURN server it can be the source.
  The default STUN list of the LiveKit server was not checked in this run.
- `internetClient` excludes private ranges. A self-hosted SFU on a LAN or
  VPN address cannot be reached by the contained worker.
  *Still open: a containment decision.* Not changed.
- While the core's proxy warning is open, the call button is disabled with
  no text (evidence finding).
  *Fixed 19 November 2026 in `frontend/src/calls.ts`.* The pending text now
  says that a proxy warning window may be open. The page cannot see the
  core's window, so the text is not specific to the warning.
- Newer SFU transport form (`m.livekit` with `url`, Synapse 1.161 and later).
  *Done 19 November 2026 in `crates/core/src/calls/readiness.rs` and
  `membership.rs`; see the ADR-0043 amendment.*

## Synapse configuration additions

Added to the disposable config only, under `.local/`:

- listener resources `client`, `federation`, `openid` on `127.0.0.1:8008`;
- `max_event_delay_duration: 24h`;
- the MSC4140 and MSC4143 experimental flags that 1.163.0 documents;
- `matrix_rtc.transports: [{type: livekit, livekit_service_url: http://127.0.0.1:6080}]`.

## Downloads (each needs the owner's explicit approval)

| File | Source | Size (bytes) | Published SHA-256 |
| --- | --- | --- | --- |
| `livekit_1.13.11_windows_amd64.zip` | github.com/livekit/livekit release v1.13.11 | 19,691,002 | `5d071ea43339f0140d24a66f0b2ff8a7bf6c4d0dceb19b30abca835670213667` |
| `element-call-0.28.2.tar.gz` | github.com/element-hq/element-call release v0.28.2 | 13,894,941 | `2ca25331b23ae5c5612ae19fc53d5074bebf37c506a52f9db05674079f0f1340` |
| `element-call-0.28.2.sha256` | same release | 19,778 | `18e318cd00b0d53e941b95a03dac6dc138d36143ca0fe73b2b48c6f317d4dca7` |
| lk-jwt-service v0.10.0: either `lk-jwt-service_linux_amd64` | github.com/element-hq/lk-jwt-service release v0.10.0 | 17,670,280 | `18bcd27802395900c3659c968a4af4baf5c7fe77444e1be2e7089228ee365432` |
| or its source tag and crates, built on Windows | same repository, crates.io | not known until resolved | from its `Cargo.lock` |
| Synapse 1.163.0 from PyPI | only if the October WSL venv is gone | stated at that time | from PyPI |

Sizes and digests are from the GitHub release API on 16 November 2026. The
first four files were downloaded on 16 November 2026 into
`.local/live-call/downloads/`; each matched its size and SHA-256. The
lk-jwt source build is not needed. The Synapse wheel is
`matrix_synapse-1.163.0-cp310-abi3-manylinux_2_28_x86_64.whl`
(8,749,194 bytes, SHA-256
`8b84a7bc1fe65a7fc369f8818d4a06f7228dc299836299000297c0a1b654e99a`) plus its
dependency wheels; it is not downloaded yet. Each
file goes into its own empty directory under `.local/live-call/`. No script
from an archive runs on the host. Element Call is served as static files by a
server kept outside the archive directory, as in the October run.

## System changes (the owner makes them, not the agent)

1. **Loopback exemption for the call worker's AppContainer.** Without it the
   worker cannot reach `127.0.0.1`. In an elevated prompt:
   `CheckNetIsolation.exe LoopbackExempt -a -n=Persephone.CallWorker`.
   Remove it after the run with `-d -n=Persephone.CallWorker`, and confirm
   with `-s`. The report records that it was set and removed. The exemption
   weakens the worker's containment on this machine while it exists.
2. **Windows Firewall prompts** for `livekit-server.exe` on loopback, if
   Windows shows one. Allow private only, or decline if loopback works
   without it.
3. No change to `.wslconfig`, the hosts file or the certificate store.

## Run steps

Pass criteria are in brackets. Each step records its observed result.

1. Spike R1 to R5 with Element Call alone and curl. [go for every risk]
2. Start Synapse, livekit-server and lk-jwt. Register two harness accounts.
   Make an encrypted room. Verify the two sides by emoji, as in the October
   run, so frame keys go only to cross-signed devices.
3. **Readiness.** Persephone shows the call button as available.
   [`call_readiness` available; no proxy prompt with no proxy set]
4. **Join from Persephone first.** [the member state event exists with a
   delayed leave id; Element Call shows a call in progress]
5. **Element Call joins.** [one LiveKit room, two participants; Persephone
   lists the peer as reachable and keyed]
6. **Audio both ways.** A test tone into one microphone at a time.
   [each side hears the other; LiveKit reports encrypted tracks]
7. **Mute and unmute** in Persephone. [the peer hears silence, then sound]
8. **Delayed leave restart.** Wait more than 60 s. [the delayed event is
   restarted about every 5 s; no early leave]
9. **Element Call leaves and rejoins.** [Persephone rotates its key; audio
   works after the rejoin]
10. **Persephone leaves.** [member content becomes `{}`; the delayed event
    is cancelled or sent]
11. **Lock during a call.** [the worker process ends at once; no leave is
    sent by Persephone; the delayed leave fires within about 8 s]
12. **Worker killed from outside** (Task Manager). [the call ends in the UI;
    the delayed leave fires; a new join starts a fresh worker]
13. **Negative control.** Remove the loopback exemption and join again.
    [the join fails closed with a code; no direct connection]
14. **Proxy prompt.** Set a SOCKS5 proxy in the profile and join.
    [the core's own dialog appears; "No" stops the join]

### Video steps (ADR-0048, added 17 November 2026, not yet run)

15. **Cameras listed.** Settings lists the cameras through a contained
    worker; no camera light comes on. [list shown; worker exits with 0]
16. **Video call from Persephone.** [member event has `m.call.intent`
    `video`; the camera light comes on; the own tile shows the camera]
17. **Video both ways.** Element Call turns its camera on. [each side sees
    the other; LiveKit reports encrypted video tracks; Persephone's tile is
    at most 640x360]
18. **Camera off and on** in the call bar. [the camera light goes out and
    the peer's tile for Persephone goes away; on again, the peer sees video]
19. **Rejoin with video.** Element Call leaves and rejoins. [Element Call
    sees Persephone's video after the key rotation, so the camera track uses
    the current own key index]
20. **Camera from an audio call.** Join with the call button, then turn the
    camera on. [intent stays `audio`; the peer sees video]
21. **Chosen camera missing.** Choose a camera, unplug it, turn the camera
    on. [the call bar says the camera is not connected; no other camera
    light comes on; audio goes on]
22. **Containment.** Record whether any mitigation must be relaxed for
    Media Foundation in the AppContainer, and the worker's peak memory with
    four remote videos against its 512 MiB Job limit.

## Ringing checks (ADR-0052, implemented, not yet run live)

The core code, the contracts and the page exist and have unit and IPC tests.
No run against a live homeserver or Element client has happened. Each step
needs a live Element Call or Element Web client beside Persephone, on
Synapse and on Merovingian.

23. **Incoming ring.** Start a call from Element in a direct chat with the
    Persephone user. Record whether Persephone shows "Incoming call" and the
    caller, and one desktop notification with the text "Incoming call". Record
    whether the notification names the caller only when notification names are
    on. [one prompt, one notification, no sound from Persephone, no join, no
    device on]
24. **Group room.** Start a call from Element in a group room. Record that
    Persephone does not ring (type `notification`) and that the room shows the
    call mark in the room list.
25. **Answer.** Answer with and without video. Record that every join check
    runs (readiness, proxy question, cross-signed keys) and that the prompt
    goes away on the join.
26. **Decline.** Decline from Persephone. Record that Element shows the
    decline and that the prompt goes away. Decline from Element in a ring that
    Persephone sent and record "Declined" in Persephone's call bar while the
    call goes on.
27. **Send.** Start a call from Persephone in a direct chat and in a group
    room. Record the events on the homeserver: type, `notification_type`,
    `m.mentions`, `m.relates_to`, `lifetime` 90000, `m.call.intent`, and that
    it is encrypted in an encrypted room.
28. **Sticky only.** Record whether any Element version sends only the sticky
    (MSC4354) notification; Persephone does not read it.
29. **Bounds.** Record that a second ring from the same user within 30 s, a
    ring with an old `sender_ts` and a ring from a room the user left do not
    show a prompt.

## Deafen and volume checks (ADR-0055, added 19 November 2026, not yet run)

Run these during step 6, with a test tone from Element Call.

30. **Volume.** Move the call bar's volume slider from 100 to 20 and back.
    [the tone gets quieter at once and comes back; at 0 it is silent; the
    peer's audio is not changed]
31. **Volume is saved.** Leave, then open Settings, Calls. [the slider shows
    the last value]; join again [the call starts at that volume]
32. **Deafen.** Unmuted, press Deafen. [the tone stops; the peer hears
    silence; the call bar shows Undeafen and Unmute]
33. **Undeafen.** [the tone comes back; the peer hears Persephone again]
34. **Undeafen keeps a mute.** Mute, deafen, undeafen. [the tone comes back;
    the peer still hears silence]
35. **Unmute while deafened.** Deafen, then press Unmute. [the tone comes
    back and the peer hears Persephone: unmute also undeafens]
36. **A peer joins while deafened.** Deafen, then let Element Call leave and
    rejoin. [no sound from the rejoined peer until undeafen; record any
    short sound at the rejoin]
37. **Deafen and rejoin.** Deafen, leave, join again. [the new call is not
    deafened and uses the saved volume]
38. **Another SFU.** With a member on another SFU (ADR-0054), deafen and
    move the volume. [that member's audio follows the same as the own SFU's
    members]

## Linux and macOS containment checks (ADR-0049, not yet run)

ADR-0049 keeps calls on Linux and macOS unverified until a live run like the
Windows run passes on each system, with these checks. The expected results
below come from the record and the current code. None of them has been
observed yet. Record each result, with the OS, kernel or macOS version and
commit, in the evidence report before any claim.

The containment step of the Windows run (Media Foundation and the Job) does
not apply to these systems.

### Linux

Run on a host with Landlock ABI 3 or newer (Linux 6.2 or newer) and on one
without it.

Descriptor check. The worker is not dumpable, so only root can read its
descriptors. Run each command with `sudo` on the test host. A permission
error, an empty list or a worker that is gone makes the check inconclusive,
not a pass; repeat it.

- `sudo ls -l /proc/<pid>/fd` lists every descriptor. Record the list.
- `sudo ss -xp | grep 'pid=<pid>,'` lists the worker's Unix sockets. For
  each one, take the peer inode (the number after `*` in the peer column)
  and find its owner: `sudo ss -xp | grep ' <peer inode> '`.
- Pass: no `/dev/video*` path in the descriptor list (portal path), and no
  worker socket whose peer is bound at the session bus address
  (`$DBUS_SESSION_BUS_ADDRESS` of the desktop user, usually
  `/run/user/<uid>/bus`) or the accessibility bus
  (`/run/user/<uid>/at-spi/bus*`), or belongs to `dbus-daemon` or
  `dbus-broker`.
  Record each peer and its process. A socket whose peer cannot be found
  is recorded and makes the check inconclusive.

1. Before the call, on the supported host only: `cat
   /sys/kernel/security/lsm` names `landlock`. On the host without
   Landlock, record the list as it is; step 4 applies there.
2. Join a call with the camera off. While it is connected, find the call
   worker's process ID (`pgrep -f persephone-call-worker`) and the process
   that started it (`ps -o ppid= -p <pid>`).
   - Record the baseline: `Seccomp_filters:` in `/proc/<ppid>/status`. The
     worker inherits these filters.
   - Expected pass: `/proc/<pid>/status` shows `NoNewPrivs: 1`,
     `Seccomp: 2` and `Seccomp_filters:` of exactly the baseline plus 4.
     Stage 1 installs two programs (kill and `ENOSYS`) and stage 2 installs
     two more (Landlock calls killed, new `AF_UNIX` sockets refused with
     `EACCES`). The baseline plus 2 means that stage 2 is missing, and the
     step fails. Stage 2 runs before the SFU connection, so a connected call
     must show it.
   - Expected pass: the worker is not dumpable, so `stat -c %U
     /proc/<pid>/environ` shows `root`.
   - Expected pass: the descriptor check (above) passes.
3. Turn the camera on. On the portal path the desktop asks for camera
   access.
   - Expected pass: a grant shows the own tile. A refusal leaves the camera
     `Unavailable` and makes no V4L2 attempt.
   - Expected pass: while the own tile shows, the descriptor check passes
     again. On the portal path the PipeWire remote's peer is the PipeWire
     daemon, which is expected. The portal path gets frames over that
     remote only, so a video node opened after the grant fails the step.
     `Seccomp_filters:` is still the baseline plus 4.
   - Expected pass: closing the call while the access dialog is open closes
     the dialog and ends the call at once.
4. Expected refusal: on a kernel without Landlock ABI 3, or with Landlock
   turned off, the join is refused with the containment text ("The call did
   not start: its process could not be contained."). No worker stays
   running.
5. Settings, Preferences, call devices. On the portal path the desktop asks
   for camera access. Expected pass: after a grant the camera menu lists
   the cameras (`pipewire:` IDs); after a refusal it lists none and the
   page shows no error. The microphone and speaker menus list the sound
   server's sources (no monitors) and sinks with `pulse:` IDs, and "System
   default".
6. Choose a microphone and a speaker that are not the system default, then
   join a call. Expected pass: `pactl list source-outputs` and `pactl list
   sink-inputs` show the worker's streams on the chosen devices, and
   `/proc/<pid>/environ` of the worker holds `PULSE_SOURCE` and
   `PULSE_SINK` with those names. Unplug or unload the chosen microphone,
   then join again. Expected refusal: the join stops with the device text
   and never uses the default microphone.

### Linux device listing (measured 19 November 2026)

The owner reported that Preferences showed no call devices on Linux after
the camera portal's access dialog. This was a partial run: the device
listing only, without the app, a call or the portal. It is not the live
containment run above.

- Host: Linux Mint 22.3 VM, kernel 6.14.0-37 (LSMs `landlock`, `yama`,
  `apparmor` and others), headless PipeWire 1.0.5 with `pipewire-pulse`
  (`libpulse0` 16.1), no sound card and no camera.
- Code: `origin/main` 3a40b30, then the fixes on this branch. Debug
  builds of `persephone-call-worker` with `CXX=clang++-21` and the pinned
  libwebrtc, with and without `dev-uncontained`.
- Method: a Python driver started the worker as
  `crates/media-broker/src/linux.rs` does (new session, `no_new_privs`,
  the call rlimits, descriptor 3 as the camera channel, only
  `XDG_RUNTIME_DIR`, `HOME` and `LANG=C`), sent `ListDevices` and decoded
  the answer. A connected socket to the user's own `pipewire-0` stood in for
  the portal's `OpenPipeWireRemote` (sent as the core does, byte `c`). A
  GStreamer `videotestsrc` node with media class `Video/Source` stood in
  for a camera. The driver and logs stayed on the VM; gdb gave the
  backtrace.

Before the fixes (3a40b30), each of these stops the listing on its own:

| Cause | Observed | What the page gets |
| --- | --- | --- |
| `RLIMIT_FSIZE` 0 from the broker | libpulse sizes its 64 MiB memfd pool with `ftruncate`; the worker dies with `SIGXFSZ` before `Devices`. `prlimit --fsize=0 pactl info` dies the same way. | "The call devices could not be listed." |
| No Landlock rule for `$XDG_RUNTIME_DIR/pulse` | With the file limit lifted, libpulse reports "Failed to create secure directory (/run/user/1001/pulse): Permission denied"; the worker sends `Failed(AudioDevice)`. | The same error |
| libwebrtc's weak `pw_*` stubs | With both lifted and a PipeWire remote, the worker dies with `SIGSEGV`: `pipewire::init` calls libwebrtc's `pw_init` stub, whose pointer is null. Also seen uncontained. Screen sharing (ADR-0053) uses the same connection code, so it was affected too (not run). | The same error |

After the fixes (file limit 256 MiB, read rule on `$XDG_RUNTIME_DIR/pulse`,
`webrtc::InitializePipeWire()` before `pw::init`), the contained worker
(containment bits 0) answered `Devices` and exited 0. It listed
`pipewire:persephone-testcam` ("TestCamera") over the PipeWire remote, and
no camera without one. The build without `dev-uncontained` still refused
to run without `no_new_privs` (`Failed(Containment)`, exit 3).

The microphone and speaker lists stayed empty: with the `index:` filter
turned off in a probe build, libwebrtc reported `index:0` "default:
auto_null.monitor" for recording and `index:0` "default: Dummy Output" and
`index:1` "Dummy Output" for playout. Its PulseAudio module gives no GUIDs,
and ADR-0043 never lists or selects `index:` IDs. The owner then chose
sound server names set at launch (ADR-0049); see the next record.

Not covered: the real camera portal and its dialog, a real camera or
sound card, the full app (core, broker and page), a call, a desktop
session, other distributions, and a kernel without Landlock.

### Linux microphones and speakers by name (measured 19 November 2026)

The owner chose sound server names set at launch (ADR-0049). This was a
partial run on the same VM and driver as the record above: the device list
and the start of a join, without the app, an SFU or a call.

- Code: this branch on top of 3a40b30, debug build of
  `persephone-call-worker` without `dev-uncontained`. Containment bits 0
  in every run.
- Devices: `pactl load-module module-null-sink sink_name=vsink` and
  `module-remap-source master=vsink.monitor source_name=vmic` (a source
  that is not a monitor). `pipewire-pulse` restarts by socket activation
  and then drops such modules, so they were loaded just before each run.
- Join runs: the driver sent `Join` with the chosen IDs to
  `wss://127.0.0.1:9/x`, which refuses connections. `Network` (4) means
  that the device check, audio start and stage 2 passed and only the SFU
  connection failed.

| Run | Result |
| --- | --- |
| `ListDevices` | Recording `pulse:vmic` "VirtualMic"; playout `pulse:vsink` "VirtualSpeaker". `vsink.monitor` is left out, so the `monitor_of_sink` field is read at the right place. Without the virtual devices: no recording device and `pulse:auto_null` "Dummy Output". |
| `pulse:vmic`, `pulse:vsink`, environment `PULSE_SOURCE=vmic`, `PULSE_SINK=vsink` | `Failed(Network)` |
| No choice, no names in the environment | `Failed(Network)` |
| `pulse:vmic`, no `PULSE_SOURCE` | `Failed(Protocol)` |
| `pulse:vmic`, `PULSE_SOURCE=vsink.monitor` | `Failed(Protocol)` |
| No choice, `PULSE_SINK=vsink` | `Failed(Protocol)` |
| `pulse:gone`, `PULSE_SOURCE=gone` | `Failed(DeviceNotFound)` |
| `pulse:vsink.monitor` as microphone, matching `PULSE_SOURCE` | `Failed(DeviceNotFound)` (monitors are not microphones) |
| `index:0` as microphone | `Failed(DeviceNotFound)` |

The client library sends a stream without a device name to the names in
the environment: with a second sink and source and `vsink` and `vmic` as
the defaults, `PULSE_SINK=vsink2 pacat` played to `vsink2`,
`PULSE_SOURCE=vmic2 parec` recorded from `vmic2`, and a plain `pacat`
played to `vsink`. libwebrtc opens such a stream at position 0 (read from
`audio_device_pulse_linux.cc`, m137); its own stream on the chosen device
was not observed, because no call connected.

Read-only `/dev/shm` (after review, same day): the worker's Landlock rule
for `/dev/shm` became read only, so no file it grows outlives it. With
that build, the contained worker listed the same devices, a
`pulse:vmic`/`pulse:vsink2` join reached `Failed(Network)`, and the
PipeWire remote listed `pipewire:persephone-testcam`. Because the worker
cannot stream without an SFU, `pacat` and `parec` ran under a Landlock
wrapper with the worker's path rules and `/dev/shm` read only: playback to
`vsink2` ran until stopped, and `parec` recorded 196608 bytes from `vmic`
in 3 s. `/dev/shm` stayed empty; PipeWire and the client library used
memfds.

Not covered: a connected call with audio on the chosen devices, the core
and broker launch path (covered by unit tests only), the page, a real
sound card, a device unplugged during a call, PulseAudio without PipeWire
(its pool may use `/dev/shm` files; the client then falls back to copying
over the socket, not measured), and the Landlock wrapper is not the
worker's seccomp filter.

### macOS

1. Before the call: `codesign -d --entitlements - <call worker>` lists only
   `com.apple.security.device.audio-input` and
   `com.apple.security.device.camera`, and `codesign -dv <call worker>`
   shows the hardened runtime. The main app and the other helpers have no
   entitlements.
2. Join a call with the camera off.
   - Expected pass: Activity Monitor's Sandbox column shows the call worker
     as sandboxed.
   - Record the Mach and I/O Kit services that the worker uses (the
     record requires this list before it is accepted).
3. Turn the camera on. Expected pass: macOS asks for camera access for
   Persephone, and a grant shows the own tile.
4. Expected refusal: a worker that reports any missing control (for example
   a build without the call profile) makes the join fail with the
   containment text. No input reaches it.

## Evidence

- A dated report `docs/evidence/live-calls.md` in the format of
  `element-web-interop.md`: inputs with hashes, versions, commit, build,
  method, results per step, limits.
- Raw logs, configs, tokens and profiles stay under `.local/live-call/`.
  Use synthetic accounts only. Redact access tokens and JWTs.
- Update `docs/planning/feasibility.md` and the ADR-0043 status only from
  the observed results.

## Not covered by this run

- TLS 1.3 to a real SFU (the `livekit-net` patch), TURN relay, IPv6.
- Linux and macOS: each needs its own run with the containment checks
  above.
- Merovingian (separate run after its probe).
- More than two participants.
- Deafen and the volume with more than one remote member.
