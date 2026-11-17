# MatrixRTC call standards: status on 16 November 2026

16 November 2026. Desk research into the standards status of Matrix audio and video calls, including group calls. No call code, server probe, or live test was done. Calls are outside the first-release scope. No ADR covers them.

## Method

- Read the upstream pull-request pages for each proposal (MSC) and the Synapse and Ruma pull requests listed below. A delegated web fetch read these pages. Where a page was only partly readable, the point is marked "summary only".
- Read the table of contents of the latest stable specification (v1.19) and the unstable specification. Only about 8% of each Client-Server API page body was read.
- Inspected the locally cached crate sources for the pinned `matrix-sdk 0.19.1` and `ruma-client-api 0.25.1`, from the root `Cargo.lock`.
- Approval counts and committee concerns were not visible without a GitHub sign-in.

## Proposal status

| MSC | Subject | Status | Source |
| --- | --- | --- | --- |
| MSC4143 | MatrixRTC core: sessions, membership, `m.rtc.slot` | Merged 21 October 2026. FCP complete 20 October 2026. Label `spec-pr-missing`. | [PR 4143](https://github.com/matrix-org/matrix-spec-proposals/pull/4143) |
| MSC4195 | LiveKit transport | Merged 21 October 2026. Label `spec-pr-missing`. | [PR 4195](https://github.com/matrix-org/matrix-spec-proposals/pull/4195) |
| MSC4196 | Voice and video calling application | Merged 21 October 2026. Label `spec-pr-missing`. | [PR 4196](https://github.com/matrix-org/matrix-spec-proposals/pull/4196) |
| MSC4354 | Sticky events (carry call membership) | Merged about 20 October 2026. Label `spec-pr-in-review` (summary only). | [PR 4354](https://github.com/matrix-org/matrix-spec-proposals/pull/4354) |
| MSC4140 | Cancellable delayed events | Merged. Merge date not confirmed: sources give 8 September, 6 October, and 13 November 2026. | [PR 4140](https://github.com/matrix-org/matrix-spec-proposals/pull/4140) |
| MSC4075 | Call invites and ringing notifications | Open. Labels include `needs-implementation` (summary only). | [PR 4075](https://github.com/matrix-org/matrix-spec-proposals/pull/4075) |

"Merged" means that the Spec Core Team accepted the proposal. It does not mean that the text is in a released specification.

## Specification releases

- `https://spec.matrix.org/latest/` showed v1.19. Its Client-Server API table of contents has no section for delayed events, MatrixRTC, sticky events, or RTC transports.
- The unstable Client-Server API has section 47, "Delayed Events", with `GET /_matrix/client/v1/delayed_events/{delayId}`, the `cancel`, `restart`, and `send` actions, and `PUT /_matrix/client/v3/rooms/{roomId}/delayed_event/{eventType}/{txnId}`.
- The part of the unstable page that was read had no mention of sticky events or `rtc/transports`. The rest of the page was not read.

## SFU discovery

- The stable endpoint is `GET /_matrix/client/v1/rtc/transports`. It needs an access token. Guests are allowed.
- The unstable endpoint is `/_matrix/client/unstable/org.matrix.msc4143/rtc/transports`.
- Synapse stabilised the endpoint and kept the unstable one. Merged 30 October 2026 in [synapse#20425](https://github.com/element-hq/synapse/pull/20425).
- Ruma added the stable path behind the `org.matrix.msc4143.stable` feature. Merged 11 November 2026 in [ruma#2665](https://github.com/ruma/ruma/pull/2665).
- The earlier `.well-known/matrix/client` field `org.matrix.msc4143.rtc_foci` was not removed. The open issue matrix-org/matrix.org#3735 reports outdated MSC4143 information in published discovery documentation.

## Pinned dependency support

Source inspection of the crates locked in the root `Cargo.lock`:

- `ruma-client-api 0.25.1`, `src/rtc/transports.rs`: the endpoint history has the unstable path only. There is no stable path.
- `ruma-client-api 0.25.1`, `src/discovery/discover_homeserver.rs`: reads `rtc_foci` as `org.matrix.msc4143.rtc_foci`, with the alias `m.rtc_foci`, when `unstable-msc4143` is enabled.
- `matrix-sdk 0.19.1`, `Cargo.toml`: the unconditional `ruma` dependency enables `unstable-msc4075`, `unstable-msc4140`, `unstable-msc4143`, and `unstable-msc4195`, among others.
- `matrix-sdk 0.19.1`, `src/room/calls.rs`: call-decline events only. The SDK has no WebRTC media engine. The `experimental-widgets` feature supports an embedded call application, such as Element Call. Persephone does not enable that feature.

Inference: a client on the current pins can use only the unstable transport path, unless it adds its own request types. This would require a dependency update or custom request types. Neither is approved.

## Server requirements for E2EE group calls

| Requirement | Standard status on the check date |
| --- | --- |
| Reliable to-device delivery for per-sender media keys | Stable specification |
| `POST /_matrix/client/v3/user/{userId}/openid/request_token` | Stable specification |
| Delayed events (MSC4140) | Accepted; in the unstable specification only |
| Sticky events (MSC4354) | Accepted; specification text in review |
| RTC transport discovery (MSC4143) | Accepted; no specification text |
| LiveKit SFU and its authorisation service (MSC4195) | Accepted; services run separately from the homeserver |
| Ringing other devices (MSC4075) | Open proposal |
| TURN credentials, `GET /_matrix/client/v3/voip/turnServer` (legacy 1:1 calls) | Stable specification |

Merovingian support for these features was not probed. Other homeservers differ. The [Tuwunel MSC compliance page](https://matrix-construct.github.io/tuwunel/development/compliance/msc.html) listed MSC4140 as accepted but not implemented.

## Project constraints (inference)

- Media from the SFU or peers is untrusted. A native WebRTC stack would decode it outside the WebAssembly guest required by [ADR-0003](../adr/0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md).
- Per-sender frame keys are secrets. [ADR-0004](../adr/0004-separate-the-key-holding-core-from-the-web-view-process.md) keeps them out of the web view. An embedded Element Call web application would conflict with this.
- Calls need a new requirement and an ADR before implementation.

## Limitations

- Upstream status changes quickly. Check it again before using this note for a decision.
- MSC4140 merge date is unresolved. MSC4354 and MSC4140 labels came from page summaries.
- Only part of each specification page body was read.
- No homeserver was tested.

## Other sources

- [This Week in Matrix, 16 October 2026](https://matrix.org/blog/2026/10/16/this-week-in-matrix-2026-10-16/)
- [This Week in Matrix, 9 October 2026](https://matrix.org/blog/2026/10/09/this-week-in-matrix-2026-10-09/)
