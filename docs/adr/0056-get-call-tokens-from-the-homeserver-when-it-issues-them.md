# Get call tokens from the homeserver when it issues them

* Status: accepted
* Deciders: project owner
* Date: 2026-11-20

Technical Story: on 20 November 2026 the call buttons showed "Cannot reach
your homeserver to check for calls." for an account on a Merovingian
homeserver that issues LiveKit tokens itself. Sync worked. The owner asked
for a decision record for the MSC4195 homeserver token flow, which
[ADR-0054](0054-connect-to-call-members-on-other-sfus.md) left out. The
owner accepted this record on 20 November 2026, after the Element Call
facts below were checked in source. The design is accepted; it is not
implemented or verified live.

## Context and Problem Statement

Persephone gets a LiveKit token only from an external JWT service: it posts
an OpenID token to `{service}/sfu/get`
([ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)).
MSC4195 also lets the homeserver issue the token. Such a homeserver advertises
a transport with the SFU `url` and no `livekit_service_url`. Persephone cannot
use that transport, and it reports a network failure instead of the real
cause. How does Persephone find and use a homeserver that issues call tokens?

Facts from the source read on 20 November 2026 (not run live):

* `matrix-sdk` 0.19.1 `Client::discover_rtc_transports()` asks only the
  unstable path
  `/_matrix/client/unstable/org.matrix.msc4143/rtc/transports`. It falls back
  to the `.well-known` `rtc_foci` member only on 404 `M_UNRECOGNIZED`. It
  does not cache an error.
* `ruma-client-api` 0.25.1 requires the `rtc_transports` member, and it
  requires `livekit_service_url` in a transport of type `livekit`. One entry
  that fails makes the whole answer fail. Version 0.25.1 has no stable path.
* `crates/core/src/calls/status.rs` maps every error of that call to
  `server_unreachable`. The join maps it to `no_call_server`.
* Merovingian with its built-in token service answers the unstable path with
  `{"rtc_transports":[{"type":"livekit","url":"wss://…"}]}`. With no call
  service, it answers `{}`. The stable path
  `GET /_matrix/client/v1/rtc/transports` answers with a `transports` member
  and also lists `m.livekit` with the same `url`. Merovingian refuses a
  configuration with both the external service and the built-in service.
* Merovingian ADR-0829 describes the token route:
  * `POST /_matrix/client/v1/rtc/livekit/get_token` and the unstable
    `/_matrix/client/unstable/io.element.msc4195/rtc/livekit/get_token`,
    with the Matrix access token.
  * The body is `{url, room_id, slot_id, member_id}`, with an optional
    `server_name`. `url` must equal the advertised SFU URL byte for byte.
  * The caller must be joined to the room by current room state.
  * The answer is `{"jwt": …}`.
  * The LiveKit room name is
    `base64nopad(SHA-256(canonical JSON [room_id, slot_id]))`. The LiveKit
    identity is the same hash over `[user_id, member_id]`. The slot
    `m.call#ROOM` is hashed as `m.call#room`.
* Merovingian ADR-0829 records that released Element Call 0.28.2 uses this
  flow for a transport with a `url` and no `livekit_service_url`.
* MSC4143 and MSC4195 were merged on 21 October 2026, with no specification
  text yet (`docs/research/matrixrtc-call-standards.md`).
* Persephone's LiveKit identity is `{user}:{device}`
  (`participant_identity` in `crates/core/src/calls/membership.rs`). It is
  the identity that lk-jwt-service gives to `/sfu/get` callers. The worker,
  the frame keys and the participant checks of ADR-0054 use it.

Facts from the Element Call source, read on 20 November 2026 (not run live).
The source is tag `v0.28.2` (commit `b4dd44b`) and the `matrix-js-sdk` commit
that it pins (`3f05dc4`):

* The default MatrixRTC mode is "compatibility". It writes legacy
  `m.call.member` state events. The other mode writes sticky events with a
  UUID member ID. The mode does not decide the token flow.
* The token flow follows the shape of the transport
  (`chooseAuthFlow`, `sdk/src/media-backend/livekit/auth/index.ts`):
  * Type `livekit` with `url` and no `livekit_service_url`: `get_token`
    through the own homeserver.
  * Type `livekit` with `livekit_service_url`, with or without `url`: the
    OpenID flow. Element Call keeps the OpenID flow for these, because older
    clients do not look for a hashed identity.
* `foci_preferred` contains the transport object as the homeserver
  advertised it. `livekit_alias` is added only to a transport with
  `livekit_service_url` (`sdk/src/matrixrtc/joinJsSdkSession.ts`).
* `matrix-js-sdk` accepts only type `livekit` as a LiveKit transport
  (`isUnstableLivekitTransport`). It ignores type `m.livekit`.
* The `get_token` body is `{url, server_name, room_id, slot_id, member_id}`.
  `server_name` is the server name of the transport's homeserver. The request
  uses only the unstable `io.element.msc4195` path. Element Call treats any
  404 as "not supported".
* For a legacy state event, `member_id` is the event's `membershipID`, or
  `{sender}:{device_id}` when that is missing. In compatibility mode,
  Element Call's own `member_id` is `{user}:{device}`. `slot_id` is
  `m.call#ROOM` when `call_id` is empty (`CallMembership.slotId`).
* The hashed identity is
  `unpadded standard base64(SHA-256(JSON.stringify([user_id, member_id])))`
  (`computeRtcIdentityRaw`).
* A member's backend identities depend on its own transports. If any of them
  has `url`, the member has the hashed identity. If any has
  `livekit_service_url`, the member has `{user}:{device}`. A member can have
  both (`CallMembership.parseFromEvent`).
* Frame keys still go by to-device message. The receiver finds the member by
  user ID and device ID. It then gives the key to every backend identity of
  that member (`RTCEncryptionManager`). Its own key goes to its own backend
  identities in the same way.

## Decision Drivers

* Calls must work on a homeserver that only issues tokens itself. Merovingian
  comes first (`AGENTS.md`). Other MSC4195 homeservers must also work.
* Persephone must meet Element Call members in the same LiveKit room, with
  frame keys that both sides use.
* ADR-0004 stays: the JWT, the keys and the access token stay in the core and
  the call worker. The page gets only codes.
* The transport list and the token answer are untrusted. They are bounded and
  checked before use.
* A call that cannot start shows the correct reason.
* No new operator is contacted.

## Considered Options

* The homeserver flow in the core, with a transport discovery of its own
* Ask each operator to run lk-jwt-service
* Wait for a `matrix-sdk` release that reads these transports
* Embed Element Call in the web view

## Decision Outcome

Chosen option: "The homeserver flow in the core, with a transport discovery
of its own", because it is the only option that makes calls work on these
homeservers without a server change and without a change to ADR-0004.

### Discovery

* The core asks for the transports with a request type of its own. It sends
  the request through the SDK client, so the access token, token refresh,
  TLS 1.3 and the profile proxy apply.
* Order:
  1. The stable path `GET /_matrix/client/v1/rtc/transports`, member
     `transports`.
  2. On 404 `M_UNRECOGNIZED`, the unstable path, member `rtc_transports`.
  3. On 404 `M_UNRECOGNIZED`, the `org.matrix.msc4143.rtc_foci` member of the
     account's `.well-known/matrix/client` document. The core reads it with
     the bounded fetch policy of ADR-0027.
* A missing list member is an empty list. An empty list does not fall back,
  as in the SDK.
* The core reads at most 16 KiB of an answer and examines at most 8 entries.
  It skips an entry that is not an object.
* Each entry is one of these. The rules follow Element Call 0.28.2:
  * Type `livekit` with `livekit_service_url`, with or without `url`: the
    OpenID flow of ADR-0043, with no change.
  * Type `livekit` or `m.livekit` with `url` and no `livekit_service_url`:
    the homeserver flow. `url` must be `wss://` with a host and at most
    `MAX_URL_BYTES` long. Loopback `ws://` is allowed only with the
    `dev-disposable-servers` feature, as for `/sfu/get` answers. The core
    keeps the advertised string with no change. An optional `server_name`
    must be the server name of the own user ID, else the entry is skipped.
  * Any other entry is skipped.
* The first usable entry in the server's order is used.
* The readiness reason follows the answer:
  * A request that cannot be made or finished, or an error status other than
    404 `M_UNRECOGNIZED`: `server_unreachable`.
  * An empty list: `no_call_server`.
  * An answer that cannot be read, or a list with no usable entry: a new code,
    `call_server_unsupported`, with the text "Your homeserver's call service
    is not one Persephone supports."

### Token

* The core posts to the stable token path. On 404 `M_UNRECOGNIZED` it posts
  to the unstable `io.element.msc4195` path, which is the only path that
  Element Call 0.28.2 uses. The request goes through the SDK client and
  follows no redirect.
* The body is the same as Element Call's:
  * `url`: the advertised string, with no change.
  * `server_name`: the server name of the own user ID.
  * `room_id`: the call's room.
  * `slot_id`: `m.call#ROOM`. Persephone writes an empty `call_id`, as
    Element Call does.
  * `member_id`: this device's `membershipID`, `{user}:{device}`.
* The core requests no OpenID token in this flow.
* The core reads at most 16 KiB of the answer. `jwt` has the same checks as a
  `/sfu/get` answer. Other members of the answer are ignored. The worker
  connects to the advertised `url`, never to a URL from the answer.
* Any failure is the existing `call_server` join error. The JWT is held in
  zeroized memory, and it never goes to the page or to a log.

### Membership and identity

* The own membership stays a legacy `m.call.member` state event with
  `membershipID` `{user}:{device}` and an empty `call_id`.
* For this flow its `foci_preferred` entry is
  `{"type":"livekit","url":<advertised string>}`, with the advertised
  `server_name` if there was one, and no `livekit_alias`. The type is always
  `livekit`, also when the chosen entry was `m.livekit`, because
  `matrix-js-sdk` ignores `m.livekit`.
* The membership reader also accepts type `livekit` with `url` as a
  transport. A member's SFU is the first acceptable entry, as ADR-0054
  says. An entry with `url` is compared by its `url` string, and an entry
  with only `livekit_service_url` by that string.
* The backend identities of a member follow Element Call:
  * If any of its acceptable entries has `url`, it has the hashed identity
    `unpadded standard base64(SHA-256(JSON [user_id, member_id]))`. The JSON
    is the two-string array exactly as `JSON.stringify` writes it.
    `member_id` is the member's `membershipID`, or `{sender}:{device_id}`
    when that is missing.
  * If any of its acceptable entries has `livekit_service_url`, it has
    `{user}:{device}`.
  * The own device uses the same rules for its own published entry.
* The core calculates the hashes. It gives the worker each member's frame
  keys under each of that member's backend identities. The worker gets only
  the identity strings.
* The core accepts a participant from a connection only when its identity
  equals a backend identity of a member whose SFU is that connection's SFU.
  Other participants are ignored, as now.
* Frame keys still go by to-device message and are matched to a member by
  user ID and device ID. The key message format does not change.
* If the worker messages change, the call protocol wire version goes up
  by one.

### Not in this decision

* Tokens for another server's SFU through the own homeserver (`server_name`
  set to another server, Merovingian ADR-0831). Other SFUs keep ADR-0054 (`/sfu/get`, with consent).
  A member on another SFU that offers only the homeserver flow stays "not
  reachable".
* Delegated delayed leave (`delegate_delayed_leave`, Merovingian ADR-0832).
  Persephone keeps its own delayed leave event (ADR-0043).
* Sticky `m.rtc.member` events (MSC4354). They stay deferred, as ADR-0054
  records.
* A return to `Client::discover_rtc_transports()` when a `matrix-sdk` release
  reads these transports. That needs a later change.

### Positive Consequences

* Calls work on homeservers that issue tokens themselves, such as Merovingian
  with its built-in service.
* No OpenID token is created in this flow, so no other service receives one.
* The reason shown for an unavailable call matches the cause.
* The stable MSC4143 path is used when the server has it.

### Negative Consequences

* The core has its own request and parser for transports, in addition to the
  SDK's. Both must be kept until the SDK reads these transports.
* One call can have two identity schemes, and one member can have both.
* The rules copy Element Call 0.28.2 and an unreleased `matrix-js-sdk`
  commit. MSC4195 has no specification text yet, so these rules can change.
  A change in Element Call can stop calls between the two clients.
* Element Call members in its sticky-event mode stay unreadable (ADR-0054).
* The Element Call facts come from reading the source. Not verified live. A
  live run needs Merovingian with its built-in service, LiveKit, Persephone
  and Element Call 0.28.2.

## Pros and Cons of the Options

### The homeserver flow in the core, with a transport discovery of its own

* Good, because it works with the server as it is.
* Good, because no OpenID token goes to another service.
* Good, because the authority of the page and the worker does not change.
* Bad, because the core repeats work that the SDK will do later.
* Bad, because hashed identities add a second identity scheme.

### Ask each operator to run lk-jwt-service

* Good, because the client does not change.
* Bad, because the operator must run one more service and keep a second copy
  of the LiveKit secret.
* Bad, because on Merovingian it replaces a membership check by room state
  with a check over OpenID and federation (Merovingian ADR-0829).
* Bad, because it does not fix the wrong reason shown for other servers.

### Wait for a `matrix-sdk` release that reads these transports

* Good, because Persephone keeps one discovery path.
* Bad, because no release date is known, and the SDK is not pinned to a git
  commit.
* Bad, because the SDK does not issue tokens or calculate identities, so most
  of the work is still needed.

### Embed Element Call in the web view

* Good, because Element Call already supports both flows.
* Bad, because the frame keys and the JWT would be in the web view. That
  breaks ADR-0004.

## Links

* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0027](0027-discover-the-homeserver-through-well-known-in-the-core.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0054](0054-connect-to-call-members-on-other-sfus.md)
* [MatrixRTC call standards research](../research/matrixrtc-call-standards.md)
* Merovingian ADR-0829, ADR-0831 and ADR-0832, in the Merovingian repository
