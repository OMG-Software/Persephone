# Connect to call members on other SFUs

* Status: proposed
* Deciders: project owner
* Date: 2026-11-19

Technical Story: on 19 November 2026 the owner asked for calls with members
whose homeserver uses another SFU ("multi-SFU"), which
[ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
left as later work.

## Context and Problem Statement

Each MatrixRTC member publishes to the SFU named first in its own
`foci_preferred`. Persephone connects only to its own homeserver's SFU, so a
member from a homeserver with another SFU is shown as "not reachable": the
user cannot hear or see them, and they cannot hear or see the user. How does
Persephone receive those members, and what does it reveal to the other SFU?

Facts from the research of 19 November 2026 (upstream source, not run live):

* Element Call opens one LiveKit connection for each distinct transport. It
  publishes on its own SFU and only subscribes on the others. One key
  provider serves all connections.
* The token for another SFU comes from that SFU's own JWT service, with an
  OpenID token from the user's own homeserver. The service checks the
  OpenID token with the user's homeserver over federation. A user from
  another homeserver gets a subscribe-only grant.
* The legacy `/sfu/get` request and its response are the same as for the
  own SFU. lk-jwt-service 0.10.0 marks `/sfu/get` as deprecated in favour of
  `/get_token`.
* The legacy participant identity `{user}:{device}` and the LiveKit room
  name are the same on every SFU. Frame keys go by to-device message and do
  not depend on the SFU.
* LiveKit 0.11.0 allows several `Room` connections in one process. They
  share one audio device module.

## Decision Drivers

* Hear and see every member of a call that Element Call can reach.
* Contact another operator only with the user's consent, because it sees the
  user's IP address and Matrix ID.
* Keep the worker's authority the same: it connects only where the core
  sends it, with a token the core got.
* Bound the connections and the work.

## Considered Options

* Subscribe-only connections to other SFUs, with consent for each server
* Ask other members to publish on the own SFU
* Keep other SFUs unreachable

## Decision Outcome

Chosen option: "Subscribe-only connections to other SFUs, with consent for
each server", because it is what Element Call does, and it changes nothing
for other members.

### Which SFUs

* The own SFU stays the one this device publishes to and the one named in
  its membership (ADR-0043).
* Another SFU is the first acceptable transport in a member's
  `foci_preferred` whose service URL string differs from the own one. The
  ADR-0043 checks apply (`https://`, length, the advertised string kept as
  it is; the comparison uses the parsed URL string, so a trailing slash
  alone does not make another SFU). The core keeps at most 8 acceptable
  transports for each member.
* The host of another SFU must be a public-looking DNS name. The core
  refuses IP address literals (IPv4 and IPv6), `localhost` and names that
  end in `.localhost`, names with no dot, and names that end in `.local`,
  `.home.arpa` or `.internal`. Case and one trailing dot are ignored. The
  same rule applies to the `wss://` host in that SFU's `/sfu/get` answer.
  This rule does not stop a public name that resolves to a private
  address.
* At most 3 other SFUs for each call. Members on further SFUs stay "not
  reachable". The first 3 by the members' order in room state are used.
* A member whose membership names the own SFU is received from the own SFU
  only. Element Call members also join other SFUs as silent subscribers, so
  the core accepts a participant, a speaker, a screen share or a video
  track from a connection only when that member's membership names the SFU
  of that connection.

### Consent

* Before the core contacts another SFU's service, it asks in its own window:
  the call has members on another call server, the server's host name, and
  that the server will see this computer's IP address and the user's Matrix
  ID. The answers are "Connect" and "Not now", and "Not now" is the
  default. On Linux the buttons have these names. On Windows the window is
  a Yes/No message box whose text says that Yes connects and No means not
  now.
* It asks once for each host in each call, one window at a time. "Not now"
  leaves those members "not reachable" for the rest of the call. Nothing is
  remembered after the call.
* With a profile proxy, the same window also says that the request for a
  call token uses the proxy, but the call connection to that server does
  not (ADR-0043 exception to ADR-0028). No other SFU is contacted without
  this consent.
* While a consent window is open, the call goes on with the members it can
  reach. The core cannot close the window when the call ends; an answer
  given after the end is ignored.

### Tokens and connection

* The core requests one OpenID token for each other SFU, when it connects,
  and posts it to that service's `/sfu/get` with the same body as for the
  own SFU. It follows no redirect. The response `url` must be `wss://`, its
  host must pass the host rule above, and the response is bounded as for
  the own SFU. A refused answer is handled like a failed token.
* The core sends the worker a new `Subscribe` command with an index (1 to 3),
  the `wss://` URL and the JWT. The worker connects as subscriber only: it
  publishes no track on that connection.
* The worker uses the same key provider and the same participant rules on
  every connection. It reports each remote participant, speaker, screen
  share and video track with its connection index (0 for the own SFU).
  The worker still plays and decodes tracks from a connection that the core
  ignores; the frame keys, not a worker filter, stop media under another
  member's identity.
* The core accepts a participant from connection *n* only when its
  membership names that SFU. Others are ignored.
* The video tile limits of ADR-0048 and ADR-0053 are for the whole call, not
  for each connection.
* A failure, a disconnect or an expired token on another SFU never ends the
  call. Those members become "not reachable". The core does not reconnect
  in this version, except when a member change brings a new SFU.
* Leaving the call, or the end of the call for any reason, disconnects every
  connection. The call protocol wire version goes up by one.

### Not in this decision

* The newer `/get_token` request. When the own or another SFU stops
  answering `/sfu/get`, a new decision is needed.
* Members that write only sticky `m.rtc.member` events (MSC4354) and hashed
  participant identities. Owner decision, 19 November 2026: they wait for a
  crates.io matrix-sdk release that reads sticky events (matrix-rust-sdk
  PR #7014, merged on `main` on 22 September 2026, in no release yet). The
  SDK is not pinned to a git commit. That support works only over Sliding
  Sync with server MSC4480 support, and it has no typed `m.rtc.member`
  content, so it needs its own decision when the release exists.

### Positive Consequences

* Members on other homeservers' SFUs can be heard and seen.
* No other operator is contacted without the user's consent.
* No change for other clients.

### Negative Consequences

* Each other SFU and its JWT service see the user's IP address, Matrix ID
  and the call's timing and packet sizes. They do not see the media.
* `/sfu/get` is deprecated upstream.
* All connections share one audio device module; behaviour with several
  connections is not measured.
* Not verified live. A live run needs two homeservers with two SFUs.

## Pros and Cons of the Options

### Subscribe-only connections to other SFUs, with consent for each server

* Good, because it matches Element Call.
* Bad, because more operators see the user.

### Ask other members to publish on the own SFU

* Good, because only one connection is needed.
* Bad, because MatrixRTC has no such request, and other clients would not
  do it.

### Keep other SFUs unreachable

* Good, because no other operator is contacted.
* Bad, because calls across homeservers with different SFUs stay one-way
  silent.

## Links

* [ADR-0028](0028-route-every-connection-through-the-profiles-socks5-proxy.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)
* [ADR-0053](0053-share-the-screen-in-calls-on-linux-and-windows.md)
