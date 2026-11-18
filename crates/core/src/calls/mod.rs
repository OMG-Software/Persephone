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
//! * [`readiness`]: whether the homeserver offers an SFU transport and TURN
//!   servers, so the page can say a call is not possible before it starts.
//! * [`devices`]: the saved microphone, speaker and camera, and the device
//!   list a contained worker reports.
//! * [`video`]: the newest picture on each video tile, kept apart from the
//!   worker's blocking event queue (ADR-0048).
//! * [`delayed`]: the delayed leave (MSC4140) that ends a membership when the
//!   core stops restarting it.
//! * [`incoming`]: the bounded to-device handler for other members' keys and
//!   the one decision that accepts or drops each.
//! * [`status`]: the page's view of a call: readiness, state, and the codes for
//!   an end or a refused join. Codes, flags and bounded IDs only.
//! * [`controller`]: the one running call. It starts the worker first, writes
//!   the membership, gets the SFU grant, rotates and sends keys, and shows the
//!   page a content-free state.
//!
//! Every limit below applies before data is kept. Malformed input is skipped
//! or refused; nothing here panics on peer data.

pub mod controller;
pub mod delayed;
pub mod devices;
pub mod encryption_keys;
pub mod incoming;
pub mod keys;
pub mod membership;
pub mod readiness;
pub mod sfu;
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
