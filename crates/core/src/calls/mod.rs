//! The Matrix side of MatrixRTC audio calls (ADR-0043).
//!
//! The core holds the Matrix session, so it does everything in a call that
//! needs Matrix authority. The call worker gets only the results: an SFU URL,
//! an SFU token, TURN credentials and frame keys. This module has no worker,
//! no audio and no UI wiring; it is the bounded, tested building blocks.
//!
//! * [`membership`]: the legacy `org.matrix.msc3401.call.member` state event.
//!   Own content is built here. Other members' events are the homeservers'
//!   word: each is checked (state key, application, expiry, HTTPS focus) and
//!   at most [`MAX_CALL_MEMBERS`] survive.
//! * [`keys`]: frame keys, their rotation (new key when a member leaves; share
//!   the current key with a new member only while it is under ten seconds
//!   old; use a new key one second after sending) and the bounded store of
//!   other members' keys.
//! * [`encryption_keys`]: the `io.element.call.encryption_keys` to-device
//!   event. It is sent Olm-encrypted, only to cross-signed devices (ADR-0005),
//!   and parsed strictly: a sender can set a key only for its own identity.
//! * [`sfu`]: the LiveKit JWT service request (`/sfu/get`) with an OpenID
//!   token. The answer must name a `wss://` URL and a well-formed JWT.
//! * [`other_sfu`]: which SFU each member is on, the other SFUs a call connects
//!   to (ADR-0054) and what the user has been asked about them.
//! * [`readiness`]: whether the homeserver offers an SFU transport and TURN
//!   servers, so the page can say a call is not possible before it starts.
//! * [`devices`]: the saved microphone, speaker and camera, and the device
//!   list a contained worker reports.
//! * [`device_test`]: what a device test's worker messages mean for the
//!   page (ADR-0059).
//! * [`video`]: the newest picture on each video tile, kept apart from the
//!   worker's blocking event queue (ADR-0048).
//! * [`delayed`]: the delayed leave (MSC4140) that ends a membership when the
//!   core stops restarting it.
//! * [`incoming`]: the bounded to-device handler for other members' keys and
//!   the one decision that accepts or drops each.
//! * [`rings`]: incoming call rings, the rooms that have a call, and the
//!   events this core sends to ring and to decline (ADR-0052).
//! * [`status`]: the page's view of a call: readiness, state, and the codes for
//!   an end or a refused join. Codes, flags and bounded IDs only.
//! * [`controller`]: the one running call. It starts the worker first, writes
//!   the membership, gets the SFU grant, rotates and sends keys, and shows the
//!   page a content-free state.
//!
//! Every limit below applies before data is kept. Malformed input is skipped
//! or refused; nothing here panics on peer data.

pub mod alone;
pub mod controller;
pub mod delayed;
pub mod device_test;
pub mod devices;
pub mod encryption_keys;
pub mod incoming;
pub mod keys;
pub mod membership;
pub mod other_sfu;
pub mod readiness;
pub mod ring_events;
pub mod rings;
pub mod screen;
pub mod sfu;
pub mod stats;
pub mod status;
pub mod video;
pub mod worker;

/// Most call members kept: examined events become at most this many members,
/// key holders and key recipients.
pub const MAX_CALL_MEMBERS: usize = 32;

/// Most call-member state events examined in one read.
pub const MAX_MEMBER_EVENTS: usize = 256;

/// Longest URL accepted from a homeserver, a member or the SFU service, in
/// bytes.
pub const MAX_URL_BYTES: usize = 2048;

/// Longest participant identity accepted from a member, in bytes.
pub const MAX_IDENTITY_BYTES: usize = 512;

/// Whether `url` has the `secure` scheme (or, only with the
/// `dev-disposable-servers` feature, the `plain` scheme to a loopback
/// literal), a host and no user information, and is within
/// [`MAX_URL_BYTES`].
pub(crate) fn is_service_url(url: &url::Url, secure: &str, plain: &str) -> bool {
    use url::Host;
    if url.as_str().len() > MAX_URL_BYTES
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
    {
        return false;
    }
    if url.scheme() == secure {
        return true;
    }
    cfg!(feature = "dev-disposable-servers")
        && url.scheme() == plain
        && match url.host() {
            Some(Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
            Some(Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
            _ => false,
        }
}

/// One LiveKit transport entry: a homeserver's advertised transport or a
/// member's `foci_preferred` entry (ADR-0056). Discovery and the membership
/// reader both read entries with [`livekit_entry`], so they cannot differ.
pub(crate) struct LiveKitEntry {
    /// The acceptable `https` `livekit_service_url`, as advertised and
    /// parsed: the entry uses the OpenID flow.
    pub service: Option<(String, url::Url)>,
    /// The acceptable `wss` `url`, as advertised and parsed.
    pub sfu: Option<(String, url::Url)>,
}

/// The advertised string and its parse, when `value` is a string of at
/// most [`MAX_URL_BYTES`] that [`is_service_url`] accepts.
fn acceptable_url(
    value: &serde_json::Value,
    secure: &str,
    plain: &str,
) -> Option<(String, url::Url)> {
    let advertised = value.as_str().filter(|raw| raw.len() <= MAX_URL_BYTES)?;
    let url = url::Url::parse(advertised)
        .ok()
        .filter(|url| is_service_url(url, secure, plain))?;
    Some((advertised.to_owned(), url))
}

/// Reads one entry by the rules of Element Call 0.28.2. `None` to skip it:
///
/// * The type must be `livekit` or `m.livekit`.
/// * An entry with `livekit_service_url` must be type `livekit` (the type
///   `matrix-js-sdk` reads) with an acceptable `https` URL. Its `url`, if
///   acceptable, is kept too.
/// * An entry without it must have an acceptable `wss` `url`.
pub(crate) fn livekit_entry(entry: &serde_json::Value) -> Option<LiveKitEntry> {
    let kind = entry.get("type")?.as_str()?;
    if kind != "livekit" && kind != "m.livekit" {
        return None;
    }
    let sfu = entry
        .get("url")
        .and_then(|url| acceptable_url(url, "wss", "ws"));
    match entry.get("livekit_service_url") {
        Some(service) => {
            if kind != "livekit" {
                return None;
            }
            Some(LiveKitEntry {
                service: Some(acceptable_url(service, "https", "http")?),
                sfu,
            })
        }
        None => Some(LiveKitEntry {
            service: None,
            sfu: Some(sfu?),
        }),
    }
}
