# Make audio calls through MatrixRTC in a contained call worker

* Status: proposed
* Deciders: project owner
* Date: 2026-11-16

Technical Story: on 16 November 2026 the owner asked for TURN and MatrixRTC
calls, including group calls. The owner chose audio-only calls first, a
separate call worker, MatrixRTC only, and the answers recorded under
"Owner choices" below.

## Context and Problem Statement

Matrix group calls use MatrixRTC (MSC4143) with a LiveKit SFU (MSC4195).
The SFU ends each DTLS-SRTP connection, so end-to-end encryption needs a
second layer: each sender encrypts every media frame with its own key and
sends that key to the other members as an Olm-encrypted to-device message.
[The standards research](../research/matrixrtc-call-standards.md) records
the status of these proposals.

A call needs a WebRTC engine. The pinned Matrix SDK has none. A WebRTC engine
decodes audio from other members in native code, and it must hold the frame
keys. The core holds sessions and keys (ADR-0004). The web view is keyless
(ADR-0004). Received media is decoded in a bounded guest (ADR-0003). Where
does the engine run, which secrets does it get, and how does it meet the
TLS and proxy rules?

## Decision Drivers

* The Matrix access token, the Olm and Megolm keys and the stores stay in
  the core (ADR-0004).
* The web view stays keyless.
* Untrusted media is not decoded in the core or the web view.
* Interoperate with current Element Call and Element X calls.
* TLS 1.3 with certificate checks on every TLS connection.
* Fail closed when a call cannot be set up as designed.

## Considered Options

* A contained call worker that runs LiveKit's Rust SDK and libwebrtc.
* WebRTC in the web view.
* WebRTC in the core.
* Element Call in an embedded web view, driven by the SDK's widget API.

## Decision Outcome

Proposed option: "a contained call worker".

### Processes and secrets

* The core does the Matrix side of a call: transport discovery, membership,
  the delayed leave, the OpenID token, the SFU token request, and the frame
  key exchange.
* The core starts one call worker process for each active call. The worker
  runs LiveKit's `Room` with WebRTC's own audio device module
  (`PlatformAudio`) for the microphone and speakers.
* The core gives the worker only: the SFU URL, the SFU access token (a
  LiveKit JWT scoped to one SFU room and one participant, valid for at most
  one hour), the TURN servers and their short-lived credentials, and the
  frame keys of the call. The Matrix access token and the Matrix
  cryptographic keys never leave the core.
* The worker and the core use the bounded frame protocol of the media
  worker, with a new set of control messages. The worker reports only
  bounded, content-free events: connected, participant joined or left,
  speaking, muted, failed.
* The web view gets content-free call state through the core's event
  stream. It never receives a token, a key, or audio.

### Containment

* Windows: an AppContainer with the `internetClient` and `microphone`
  capabilities only, in a Job with a memory limit and kill-on-close, and the
  media worker's mitigation policy where audio still works. Each mitigation
  that audio needs to turn off is measured and recorded. The worker checks
  its own containment and refuses to start a call when a control is missing.
* The AppContainer does not deny network access; the worker needs the
  network. The containment limits file system, registry, process and
  window-system access, not the destinations the worker can reach.
* Linux and macOS: calls are refused until each OS has its own reviewed
  profile and recorded checks.

### Matrix side

* Membership: the legacy `org.matrix.msc3401.call.member` state event,
  which Element Call writes and reads in its default compatibility mode,
  with the LiveKit focus. The core schedules a delayed leave (MSC4140) and
  restarts it while the call runs, so a crash ends the membership.
* The member event follows what matrix-js-sdk writes (read 16 November
  2026): state key `_{user}_{device}_m.call`, `application` `m.call`,
  empty `call_id`, `scope` `m.room`, `expires` 4 hours, `focus_active`
  `{"type":"livekit","focus_selection":"multi_sfu"}`, one
  `foci_preferred` entry with this homeserver's LiveKit service URL,
  `membershipID` `{user}:{device}`, and `m.call.intent` `audio`. The
  LiveKit participant identity is `{user}:{device}`. The delayed leave
  is 8 seconds, restarted every 5 seconds. Leaving writes `{}`.
* Each member publishes to the SFU named first in its own
  `foci_preferred` ("multi-SFU"). This first version connects only to
  the SFU of this homeserver. Members who publish to another SFU are shown
  as not reachable. Connecting to more than one SFU is later work.
* Only memberships whose state key names the event's sender, whose
  service URL is `https://`, and that have not expired are used. At most
  32 members are kept.
* Sticky `m.rtc.member` events (MSC4354) are not written. The pinned SDK
  does not compile them. Reading them is later work.
* SFU discovery: `discover_rtc_transports()` (the unstable
  `rtc/transports` path, then the `.well-known` `rtc_foci` fallback).
  TURN credentials: `GET /_matrix/client/v3/voip/turnServer`.
* SFU token: an OpenID token from the homeserver, sent to the
  transport's `/sfu/get`. The response URL must be `wss://`.
* Frame keys: a new 32-byte random key for each rotation, with indices
  `(i + 1) mod 256`. Rotate when a member leaves. Share the current key
  with new members when it is less than 10 s old, else rotate. Use a new
  key 1 s after it is sent.
* "Use" means two steps in the worker: store the key at its index, then
  set the sender's frame cryptor to that index. Native libwebrtc keeps the
  sender's index apart from the key store, at 0 until it is set. The live
  test of 16 November 2026 found the second step missing: every frame went
  out under the first key, so later joiners heard nothing and a rotation
  did not take effect.
* Keys are sent as `io.element.call.encryption_keys` to-device events,
  encrypted with Olm, only to devices that are in the call and are signed by
  their owner's cross-signing identity (ADR-0005). Keys are accepted only
  from such devices, and only when the event arrived encrypted. A member
  whose device is not cross-signed cannot hear this user, and this user
  cannot hear them; the page says so.
* Sending Olm-encrypted custom to-device events needs the
  `experimental-send-custom-to-device` feature of `matrix-sdk`. The
  version does not change.

### Audio devices

* Owner request, 16 November 2026: choose the microphone and the speaker
  in Persephone. A camera choice waits for video calls, which are not
  approved.
* The core never opens an audio device. To list devices, it starts a
  contained call worker through the same host and `Ready` check as a call
  and sends `ListDevices` as the first message. The worker replies once
  with at most 32 microphones and 32 speakers, then exits. It does not
  record, play or connect.
* A device is the libwebrtc device ID (on Windows the endpoint ID) and the
  OS name. IDs that LiveKit makes from a list position (`index:N`) are not
  listed, because a hot-plug can move them to another device. Names are
  untrusted text: the worker removes control characters and cuts them to
  256 bytes, the core normalizes them like display names (control and
  bidirectional formatting characters become U+FFFD), and the page shows
  them as plain text.
* A list request during a call is allowed. Its worker creates WebRTC's
  audio device module and enumerates devices, but never starts recording
  or playout, so it does not compete with the call for a device. This was
  read from LiveKit 0.11's `PlatformAudio::new`, not measured.
* The choice is a profile record in the encrypted store, like the proxy.
  It applies from the next call. `Join` carries the chosen IDs; none means
  the system default.
* A chosen device that is not connected stops the join with
  `DeviceNotFound`. The worker never falls back to another device, because
  that device could be a microphone the user did not choose. A record that
  cannot be read stops the join too.

### Session lifecycle

* A running call holds no Matrix client. Each SDK operation of the call
  (a delayed-leave restart, a membership refresh, a member read, a key
  send, the leave) takes its own session lease, like the sync loop. It is
  refused when the call stopped, when the session admits no more work, or
  when the session generation changed.
* The proxy question is asked under no lease, so a lock never waits for an
  answer. The join itself runs under one lease.
* Lock, idle lock, sign-out, a session the homeserver ended, and exit stop
  the call when admission closes: the core kills the worker at once, so
  audio stops, and sends no Matrix leave. The delayed leave ends the
  membership on the server within about 8 seconds.
* A leave during a join cancels the join, which undoes its steps.

### Owner choices

* TLS: `livekit-net` compiles TLS 1.2 in and has no setting to turn it
  off. A vendored copy under `third_party/`, used through
  `[patch.crates-io]`, builds a TLS 1.3-only rustls configuration with
  certificate checks. The vendored Rust `libwebrtc` crate raises the
  frame key ring from 255 to 256 slots, so that Element Call's key index
  255 works. Each patch is small and is reapplied for each LiveKit update.
* libwebrtc: `tools/dev.py` downloads the pinned prebuilt archive into
  `.local/`, checks its SHA-256 against a committed value, and gives its
  path to the build in `LK_CUSTOM_WEBRTC`. A wrong hash stops the build.
  The build script's own unchecked download is not used. The committed
  values are the SHA-256 digests that GitHub reported for the
  `webrtc-a545de4` release assets on 16 November 2026. They detect a
  changed archive after that date; they do not prove who built it.
* Proxy: WebRTC media and LiveKit signalling cannot use a SOCKS5 proxy.
  When the profile has a proxy, the core asks in its own window before a
  call: the call connects directly, and the SFU and TURN server see this
  computer's IP address. No call connects directly without that
  confirmation. This is an exception to ADR-0028.
* Audio only. Video and screen sharing are later decisions.

### Positive Consequences

* Audio decoding, network-facing WebRTC code and the frame keys run in a
  process that holds no Matrix credentials and no Matrix keys.
* The frame keys follow the same device-trust rule as room keys.
* One protocol and one SFU path for one-to-one and group calls.

### Negative Consequences

* A large native dependency: Google's WebRTC through LiveKit's fork, as a
  prebuilt static library. Its security fixes reach Persephone only through
  LiveKit releases.
* The worker can reach any network address. A compromised worker could
  send the call's audio and keys anywhere.
* Opus audio is decoded in native code, outside the WebAssembly guest of
  ADR-0003. ADR-0003 keeps governing images.
* Two vendored patches to maintain.
* The SFU and TURN operators see who is in a call, when, from which IP
  address, and the size and timing of the packets. They do not see the
  audio.
* Calls interoperate only with clients and servers that support the legacy
  membership event and delayed events.
* No call on Linux or macOS until their profiles exist.

## Pros and Cons of the Options

### A contained call worker

* Good, because no Matrix secret reaches the code that handles media.
* Good, because a worker crash ends only the call.
* Bad, because it adds a new process, protocol and containment profile on
  each OS.

### WebRTC in the web view

* Good, because the web view already has WebRTC, capture and playback.
* Bad, because the frame keys would enter the web view, against ADR-0004.

### WebRTC in the core

* Good, because it needs the least plumbing.
* Bad, because the process with every secret would decode untrusted media.

### Element Call in an embedded web view

* Good, because it is the reference client.
* Bad, because it loads a remote or bundled web application with Matrix
  authority through the widget API, against ADR-0004 and the rule against
  external assets.

## Links

* [MatrixRTC call standards research](../research/matrixrtc-call-standards.md)
* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)
* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
* [ADR-0028](0028-route-every-connection-through-the-profiles-socks5-proxy.md)
* [Windows sandbox evidence](../evidence/windows-sandbox.md)
* [Live call run with Element Call](../evidence/live-calls.md)
