//! The legacy MatrixRTC member state event (ADR-0043).
//!
//! Element Call reads and writes `org.matrix.msc3401.call.member` in its
//! default compatibility mode. One event per device, with the state key
//! `_{user}_{device}_m.call`. Own content is built exactly. Other members'
//! events are untrusted: [`active_members`] keeps only those that are
//! well-formed, unexpired, written by the user they name, and that name a
//! LiveKit transport: an `https` JWT service or a `wss` SFU (ADR-0056).

use super::{MAX_CALL_MEMBERS, MAX_IDENTITY_BYTES, MAX_MEMBER_EVENTS, livekit_entry};
use matrix_sdk::ruma::{
    DeviceId, OwnedDeviceId, OwnedUserId, UserId,
    events::StateEventType,
    serde::{Base64, base64::Standard},
};
use matrix_sdk_base::deserialized_responses::RawAnySyncOrStrippedState;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use url::Url;

/// The state event type.
pub const EVENT_TYPE: &str = "org.matrix.msc3401.call.member";

/// How long an own membership lasts without a refresh.
pub const MEMBERSHIP_EXPIRES: Duration = Duration::from_secs(4 * 60 * 60);

/// The delayed leave (MSC4140) fires this long after the last restart.
pub const DELAYED_LEAVE: Duration = Duration::from_secs(8);

/// Restart the delayed leave this often while the call runs.
pub const DELAYED_LEAVE_RESTART: Duration = Duration::from_secs(5);

/// Refresh the membership this often, well inside [`MEMBERSHIP_EXPIRES`].
pub const MEMBERSHIP_REFRESH: Duration = Duration::from_secs(55 * 60);

/// Longest `expires` accepted from another member.
const MAX_EXPIRES_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Most `foci_preferred` entries examined for one member.
pub const MAX_FOCI: usize = 8;

/// Longest device ID accepted from another member, in bytes.
const MAX_DEVICE_ID_BYTES: usize = 128;

/// The state key of a device's own membership.
pub fn own_state_key(user_id: &UserId, device_id: &DeviceId) -> String {
    format!("_{user_id}_{device_id}_m.call")
}

/// The LiveKit participant identity of a device on a JWT service (the
/// OpenID flow), and the `membershipID` this device writes.
pub fn participant_identity(user_id: &UserId, device_id: &DeviceId) -> String {
    format!("{user_id}:{device_id}")
}

/// The MSC4195 LiveKit participant identity that a homeserver token gives
/// (ADR-0056): unpadded standard base64 of the SHA-256 of the JSON array
/// `[user_id, member_id]`, as `JSON.stringify` writes it.
pub fn hashed_identity(user_id: &UserId, member_id: &str) -> String {
    use sha2::{Digest, Sha256};
    // Two strings cannot fail to serialize. serde_json escapes strings as
    // `JSON.stringify` does: the short escapes, `\u00XX` in lower case for
    // the other controls, and nothing else.
    let json = serde_json::to_string(&[user_id.as_str(), member_id]).unwrap_or_default();
    Base64::<Standard, _>::new(Sha256::digest(json.as_bytes()).to_vec()).encode()
}

/// The content of this device's membership event.
///
/// `focus` is the one `foci_preferred` entry, from `SfuService::focus`
/// (ADR-0056). `created_ts` is set on a refresh, so the join time survives
/// it.
/// `m.call.intent` is `"video"` when the call was joined with `video`, else
/// `"audio"`. It is fixed at the join and not changed by the camera button.
pub fn own_membership(
    device_id: &DeviceId,
    user_id: &UserId,
    focus: &Value,
    created_ts: Option<u64>,
    video: bool,
) -> Value {
    let mut content = json!({
        "application": "m.call",
        "call_id": "",
        "scope": "m.room",
        "device_id": device_id.as_str(),
        "expires": MEMBERSHIP_EXPIRES.as_millis() as u64,
        "focus_active": { "type": "livekit", "focus_selection": "multi_sfu" },
        "foci_preferred": [focus],
        "membershipID": participant_identity(user_id, device_id),
        "m.call.intent": if video { "video" } else { "audio" },
    });
    if let (Some(ts), Some(map)) = (created_ts, content.as_object_mut()) {
        map.insert("created_ts".to_owned(), json!(ts));
    }
    content
}

/// The content that ends a membership.
pub fn leave_content() -> Value {
    json!({})
}

/// A member's device in the call, after validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveMember {
    pub user_id: OwnedUserId,
    pub device_id: OwnedDeviceId,
    /// The member's `membershipID`, or `{user}:{device}` without one. Two
    /// devices may share it, so leave and join changes go by the device
    /// together with this identity; frame keys and worker participants go by
    /// `{user}:{device}` ([`participant_identity`]) and
    /// [`ActiveMember::backend_identities`].
    pub identity: String,
    /// The LiveKit participant identities the member may have on an SFU
    /// (ADR-0056): the hashed identity when a transport has `url`, and
    /// `{user}:{device}` when one has `livekit_service_url`. One that another
    /// member also has (two devices with one membership ID) is left out, so
    /// it may be empty.
    pub backend_identities: Vec<String>,
    /// The first acceptable LiveKit transport of the member, which is where
    /// it publishes (ADR-0043). It is `transports[0]`.
    pub service_url: Url,
    /// Every acceptable LiveKit transport in `foci_preferred`, in the
    /// member's order, at most [`MAX_FOCI`] examined, each as the URL it is
    /// compared by: `livekit_service_url` (`https`) when the entry has one,
    /// else `url` (`wss`). The URL has at most [`MAX_URL_BYTES`] bytes and no
    /// user information. ADR-0054 picks another SFU from here.
    pub transports: Vec<Url>,
    pub joined_ts: u64,
    pub expires_ts: u64,
}

/// A call-member state event as read from the room, before validation.
#[derive(Clone, Debug)]
pub struct MemberEvent {
    pub state_key: String,
    pub sender: OwnedUserId,
    pub origin_server_ts: u64,
    pub content: Value,
}

/// The valid, unexpired members among `events`, oldest join first, at most
/// [`MAX_CALL_MEMBERS`]. Examines at most [`MAX_MEMBER_EVENTS`] events.
/// Malformed events are skipped.
pub fn active_members(
    events: impl IntoIterator<Item = MemberEvent>,
    now_ms: u64,
) -> Vec<ActiveMember> {
    let mut newest: HashMap<(OwnedUserId, OwnedDeviceId), ActiveMember> = HashMap::new();
    for event in events.into_iter().take(MAX_MEMBER_EVENTS) {
        let Some(member) = parse_member(&event, now_ms) else {
            continue;
        };
        let key = (member.user_id.clone(), member.device_id.clone());
        match newest.get(&key) {
            Some(existing) if existing.joined_ts >= member.joined_ts => {}
            _ => {
                newest.insert(key, member);
            }
        }
    }
    let mut members: Vec<ActiveMember> = newest.into_values().collect();
    members.sort_by(|a, b| {
        a.joined_ts
            .cmp(&b.joined_ts)
            .then_with(|| a.identity.cmp(&b.identity))
    });
    members.truncate(MAX_CALL_MEMBERS);
    // Two devices of one user may name the same membership ID, and so the
    // same hashed identity. Such an identity cannot say which device it
    // is, so neither keeps it.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for member in &members {
        for id in &member.backend_identities {
            *seen.entry(id.clone()).or_default() += 1;
        }
    }
    for member in &mut members {
        member
            .backend_identities
            .retain(|id| seen.get(id).copied() == Some(1));
    }
    members
}

fn valid_device_id(device: &str) -> bool {
    !device.is_empty()
        && device.len() <= MAX_DEVICE_ID_BYTES
        && device.bytes().all(|b| b.is_ascii_graphic())
}

/// One acceptable `foci_preferred` entry (ADR-0056).
struct Focus {
    /// The URL the entry is compared by.
    url: Url,
    /// It has an acceptable `url`: the member may use the hashed identity.
    has_url: bool,
    /// It has `livekit_service_url`: the member may use `{user}:{device}`.
    has_service: bool,
}

/// The acceptable transport of one `foci_preferred` entry, by the rules of
/// [`livekit_entry`], which discovery uses too.
fn focus(entry: &Value) -> Option<Focus> {
    let entry = livekit_entry(entry)?;
    Some(Focus {
        has_url: entry.sfu.is_some(),
        has_service: entry.service.is_some(),
        url: entry.service.or(entry.sfu)?.1,
    })
}

fn parse_member(event: &MemberEvent, now_ms: u64) -> Option<ActiveMember> {
    let content = event.content.as_object().filter(|c| !c.is_empty())?;
    if content.get("application")?.as_str()? != "m.call"
        || content.get("scope")?.as_str()? != "m.room"
        || !content.get("call_id")?.as_str()?.is_empty()
    {
        return None;
    }
    let device = content.get("device_id")?.as_str()?;
    if !valid_device_id(device) {
        return None;
    }
    let device_id = OwnedDeviceId::from(device);
    // The state key must name the sender: another user's key is a forgery.
    let exact = own_state_key(&event.sender, &device_id);
    let bare = format!("{}_{}_m.call", event.sender, device_id);
    if event.state_key != exact && event.state_key != bare {
        return None;
    }
    let expires = content.get("expires")?.as_u64()?;
    if expires == 0 || expires > MAX_EXPIRES_MS {
        return None;
    }
    let joined_ts = content
        .get("created_ts")
        .and_then(Value::as_u64)
        .map_or(event.origin_server_ts, |c| c.min(event.origin_server_ts));
    let expires_ts = joined_ts.saturating_add(expires);
    if expires_ts <= now_ms {
        return None;
    }
    // Every acceptable LiveKit transport, in the member's order, at most
    // [`MAX_FOCI`] examined. One that is not acceptable is skipped; a member
    // with none is not used.
    let foci: Vec<Focus> = content
        .get("foci_preferred")?
        .as_array()?
        .iter()
        .take(MAX_FOCI)
        .filter_map(focus)
        .collect();
    let service_url = foci.first()?.url.clone();
    let identity = match content.get("membershipID").and_then(Value::as_str) {
        Some(id) if id.len() <= MAX_IDENTITY_BYTES && !id.chars().any(char::is_control) => {
            id.to_owned()
        }
        _ => participant_identity(&event.sender, &device_id),
    };
    // As Element Call 0.28.2: the hash of the sender and the membership ID
    // for a homeserver token, the sender and the device for a JWT service.
    let mut backend_identities = Vec::with_capacity(2);
    if foci.iter().any(|f| f.has_url) {
        backend_identities.push(hashed_identity(&event.sender, &identity));
    }
    if foci.iter().any(|f| f.has_service) {
        backend_identities.push(participant_identity(&event.sender, &device_id));
    }
    let transports = foci.into_iter().map(|f| f.url).collect();
    Some(ActiveMember {
        user_id: event.sender.clone(),
        device_id,
        identity,
        backend_identities,
        service_url,
        transports,
        joined_ts,
        expires_ts,
    })
}

/// The call members of `room` now, from its stored state.
pub async fn read_members(
    room: &matrix_sdk::Room,
    now_ms: u64,
) -> matrix_sdk::Result<Vec<ActiveMember>> {
    #[derive(serde::Deserialize)]
    struct Loose {
        state_key: String,
        sender: OwnedUserId,
        origin_server_ts: u64,
        content: Value,
    }
    let raw = room
        .get_state_events(StateEventType::from(EVENT_TYPE))
        .await?;
    let events = raw.into_iter().filter_map(|event| match event {
        RawAnySyncOrStrippedState::Sync(raw) => {
            let loose: Loose = serde_json::from_str(raw.json().get()).ok()?;
            Some(MemberEvent {
                state_key: loose.state_key,
                sender: loose.sender,
                origin_server_ts: loose.origin_server_ts,
                content: loose.content,
            })
        }
        RawAnySyncOrStrippedState::Stripped(_) => None,
    });
    Ok(active_members(events, now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{device_id, user_id};

    const NOW: u64 = 1_000_000_000_000;
    const SERVICE: &str = "https://livekit.example.org/jwt";

    fn url() -> Url {
        Url::parse(SERVICE).unwrap()
    }

    /// The OpenID flow focus for `service`, as `SfuService::focus` makes it.
    fn openid(service: &str) -> Value {
        json!({ "type": "livekit", "livekit_service_url": service, "livekit_alias": "!r:example.org" })
    }

    fn event(sender: &str, device: &str, ts: u64, content: Value) -> MemberEvent {
        MemberEvent {
            state_key: format!("_{sender}_{device}_m.call"),
            sender: OwnedUserId::try_from(sender).unwrap(),
            origin_server_ts: ts,
            content,
        }
    }

    fn good(sender: &str, device: &str, ts: u64) -> MemberEvent {
        let content = own_membership(
            &OwnedDeviceId::from(device),
            &OwnedUserId::try_from(sender).unwrap(),
            &openid(SERVICE),
            None,
            false,
        );
        event(sender, device, ts, content)
    }

    /// GIVEN own membership data WHEN built THEN it is the exact legacy event content.
    #[test]
    fn given_own_membership_then_exact_content() {
        let content = own_membership(
            device_id!("DEV"),
            user_id!("@a:example.org"),
            &openid(SERVICE),
            None,
            false,
        );
        assert_eq!(
            content,
            json!({
                "application": "m.call",
                "call_id": "",
                "scope": "m.room",
                "device_id": "DEV",
                "expires": 14_400_000,
                "focus_active": { "type": "livekit", "focus_selection": "multi_sfu" },
                "foci_preferred": [{
                    "type": "livekit",
                    "livekit_service_url": SERVICE,
                    "livekit_alias": "!r:example.org",
                }],
                "membershipID": "@a:example.org:DEV",
                "m.call.intent": "audio",
            })
        );
        let refreshed = own_membership(
            device_id!("DEV"),
            user_id!("@a:example.org"),
            &openid(SERVICE),
            Some(42),
            false,
        );
        assert_eq!(refreshed["created_ts"], 42);
        let video = own_membership(
            device_id!("DEV"),
            user_id!("@a:example.org"),
            &openid(SERVICE),
            Some(42),
            true,
        );
        assert_eq!(video["m.call.intent"], "video");
        assert_eq!(refreshed["m.call.intent"], "audio");
        assert_eq!(
            video["created_ts"], 42,
            "a video refresh keeps the join time"
        );
        let homeserver = json!({ "type": "livekit", "url": "wss://sfu.example.org" });
        let bare = own_membership(
            device_id!("DEV"),
            user_id!("@a:example.org"),
            &homeserver,
            None,
            false,
        );
        assert_eq!(
            bare["foci_preferred"],
            json!([homeserver]),
            "the focus is written as given"
        );
        assert_eq!(leave_content(), json!({}));
        assert_eq!(
            own_state_key(user_id!("@a:example.org"), device_id!("DEV")),
            "_@a:example.org_DEV_m.call"
        );
    }

    /// GIVEN an own event WHEN read back THEN it is an active member with the participant identity.
    #[test]
    fn given_own_event_when_read_then_active_member() {
        let members = active_members([good("@a:example.org", "DEV", NOW - 1000)], NOW);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].identity, "@a:example.org:DEV");
        assert_eq!(members[0].service_url, url());
        assert_eq!(members[0].expires_ts, NOW - 1000 + 14_400_000);
    }

    /// GIVEN a state key naming another user WHEN read THEN the event is skipped.
    #[test]
    fn given_forged_state_key_for_another_user_then_skipped() {
        let mut forged = good("@mallory:example.org", "DEV", NOW);
        forged.state_key = own_state_key(user_id!("@alice:example.org"), device_id!("DEV"));
        assert!(active_members([forged], NOW).is_empty());
    }

    /// GIVEN the state key without its leading underscore WHEN read THEN it is accepted.
    #[test]
    fn given_state_key_without_underscore_then_accepted() {
        let mut e = good("@a:example.org", "DEV", NOW);
        e.state_key = "@a:example.org_DEV_m.call".to_owned();
        assert_eq!(active_members([e], NOW).len(), 1);
    }

    /// GIVEN an expired membership WHEN read THEN it is skipped; one second before expiry it is kept.
    #[test]
    fn given_expired_membership_then_skipped() {
        let e = good("@a:example.org", "DEV", NOW - 14_400_000);
        assert!(active_members([e.clone()], NOW).is_empty());
        assert_eq!(active_members([e], NOW - 1).len(), 1);
    }

    /// GIVEN an empty event (a leave) WHEN read THEN it is skipped.
    #[test]
    fn given_leave_event_then_skipped() {
        let e = event("@a:example.org", "DEV", NOW, json!({}));
        assert!(active_members([e], NOW).is_empty());
    }

    /// GIVEN a plain http, user-info or non-url service WHEN read THEN the event is skipped.
    #[test]
    fn given_unsafe_service_url_then_skipped() {
        let mut urls = vec![
            "https://u:p@livekit.example.org/jwt",
            "wss://livekit.example.org",
            "not a url",
        ];
        if !cfg!(feature = "dev-disposable-servers") {
            urls.push("http://livekit.example.org/jwt");
            urls.push("http://127.0.0.1:8080");
        }
        for bad in urls {
            let mut e = good("@a:example.org", "DEV", NOW);
            e.content["foci_preferred"][0]["livekit_service_url"] = json!(bad);
            assert!(active_members([e], NOW).is_empty(), "{bad}");
        }
    }

    /// GIVEN a member with several transports, one not acceptable, one of another type
    /// WHEN read THEN the acceptable LiveKit ones are kept in the member's order.
    #[test]
    fn given_several_transports_then_acceptable_ones_are_kept_in_order() {
        let mut e = good("@a:example.org", "DEV", NOW);
        e.content["foci_preferred"] = json!([
            {"type": "livekit", "livekit_service_url": "https://u:p@bad.example/jwt"},
            {"type": "other", "livekit_service_url": "https://other-type.example"},
            {"type": "livekit", "livekit_service_url": "https://first.example/jwt"},
            {"type": "livekit", "livekit_service_url": "https://second.example/jwt"},
            {"type": "livekit", "livekit_service_url": 7},
            {"type": "m.livekit", "url": "wss://third.example"},
        ]);
        let members = active_members([e], NOW);
        let urls: Vec<&str> = members[0].transports.iter().map(Url::as_str).collect();
        assert_eq!(
            urls,
            [
                "https://first.example/jwt",
                "https://second.example/jwt",
                "wss://third.example/"
            ]
        );
        assert_eq!(members[0].service_url.as_str(), "https://first.example/jwt");
    }

    /// GIVEN a member with more than the examined number of transports
    /// WHEN read THEN only the first entries are examined.
    #[test]
    fn given_too_many_transports_then_later_ones_are_ignored() {
        let mut e = good("@a:example.org", "DEV", NOW);
        let mut list: Vec<Value> = (0..MAX_FOCI)
            .map(|i| json!({"type": "livekit", "livekit_service_url": format!("https://s{i}.example")}))
            .collect();
        list.push(json!({"type": "livekit", "livekit_service_url": "https://late.example"}));
        e.content["foci_preferred"] = Value::Array(list);
        let members = active_members([e], NOW);
        assert_eq!(members[0].transports.len(), MAX_FOCI);
        assert!(
            members[0]
                .transports
                .iter()
                .all(|u| u.host_str() != Some("late.example"))
        );
    }

    /// GIVEN the homeserver and OpenID focus forms, alone and mixed WHEN read
    /// THEN the first acceptable one in the member's order is used, and an
    /// entry with both URLs is compared by its service URL.
    #[test]
    fn given_homeserver_focus_form_then_accepted() {
        let sfu = json!({ "type": "m.livekit", "url": "wss://sfu.example.org" });
        let legacy = json!({
            "type": "livekit", "livekit_service_url": "https://legacy.example.org/jwt"
        });
        let both = json!({
            "type": "livekit", "url": "wss://sfu.example.org",
            "livekit_service_url": "https://both.example.org/jwt"
        });
        let cases = [
            (vec![sfu.clone()], "wss://sfu.example.org/"),
            (
                vec![json!({ "type": "livekit", "url": "wss://sfu.example.org:7443" })],
                "wss://sfu.example.org:7443/",
            ),
            (
                vec![legacy.clone(), sfu.clone()],
                "https://legacy.example.org/jwt",
            ),
            (vec![sfu.clone(), legacy.clone()], "wss://sfu.example.org/"),
            (vec![both], "https://both.example.org/jwt"),
            // A malformed entry is passed over for a later good one.
            (
                vec![json!({ "type": "m.livekit", "url": 5 }), legacy.clone()],
                "https://legacy.example.org/jwt",
            ),
            (
                vec![
                    json!({ "type": "m.livekit", "url": "https://x.example.org" }),
                    sfu,
                ],
                "wss://sfu.example.org/",
            ),
        ];
        for (foci, expected) in cases {
            let mut e = good("@a:example.org", "DEV", NOW);
            e.content["foci_preferred"] = json!(foci);
            let members = active_members([e], NOW);
            assert_eq!(members[0].service_url.as_str(), expected);
        }
    }

    /// GIVEN malformed focus entries only WHEN read THEN the event is skipped.
    #[test]
    fn given_malformed_focus_then_skipped() {
        let mut bad = vec![
            json!({ "type": "m.livekit" }),
            json!({ "type": "m.livekit", "url": "not a url" }),
            json!({ "type": "m.livekit", "url": "wss://u:p@x.example.org" }),
            json!({ "type": "m.livekit", "url": "https://x.example.org" }),
            json!({ "type": "m.livekit", "livekit_service_url": "https://x.example.org" }),
            json!({ "type": "livekit", "url": "https://x.example.org" }),
            json!({ "type": "livekit", "livekit_service_url": 5, "url": "wss://x.example.org" }),
            json!({ "type": "livekit", "url": format!("wss://x.example.org/{}", "a".repeat(super::super::MAX_URL_BYTES)) }),
        ];
        if !cfg!(feature = "dev-disposable-servers") {
            bad.push(json!({ "type": "livekit", "url": "ws://127.0.0.1:7880" }));
        }
        for bad in bad {
            let mut e = good("@a:example.org", "DEV", NOW);
            e.content["foci_preferred"] = json!([bad]);
            assert!(active_members([e], NOW).is_empty(), "{bad}");
        }
    }

    /// GIVEN the MSC4195 example WHEN hashed THEN the identity is the one
    /// the proposal gives.
    #[test]
    fn given_msc4195_example_then_its_hashed_identity() {
        assert_eq!(
            hashed_identity(user_id!("@alice:example.com"), "memberABC"),
            "M3VnsLXrkbxIDINXO64u8PZzFyD9ZYFiQULR2dshWYs"
        );
    }

    /// GIVEN each focus form WHEN read THEN the backend identities follow
    /// Element Call: hashed with the membership ID for `url`, the sender and
    /// device for `livekit_service_url`, and both for an entry with both.
    #[test]
    fn given_each_focus_form_then_its_backend_identities() {
        let user = user_id!("@a:example.org");
        let hashed = hashed_identity(user, "@a:example.org:DEV");
        let cases = [
            (
                json!([openid(SERVICE)]),
                vec!["@a:example.org:DEV".to_owned()],
            ),
            (
                json!([{ "type": "livekit", "url": "wss://sfu.example.org" }]),
                vec![hashed.clone()],
            ),
            (
                json!([{ "type": "livekit", "url": "wss://sfu.example.org",
                         "livekit_service_url": SERVICE }]),
                vec![hashed.clone(), "@a:example.org:DEV".to_owned()],
            ),
            (
                json!([{ "type": "m.livekit", "url": "wss://sfu.example.org" }, openid(SERVICE)]),
                vec![hashed, "@a:example.org:DEV".to_owned()],
            ),
        ];
        for (foci, expected) in cases {
            let mut e = good("@a:example.org", "DEV", NOW);
            e.content["foci_preferred"] = foci.clone();
            assert_eq!(
                active_members([e], NOW)[0].backend_identities,
                expected,
                "{foci}"
            );
        }

        // The hash takes the membership ID; the OpenID identity does not.
        let mut e = good("@a:example.org", "DEV", NOW);
        e.content["membershipID"] = json!("custom-id");
        e.content["foci_preferred"] = json!([
            { "type": "livekit", "url": "wss://sfu.example.org", "livekit_service_url": SERVICE }
        ]);
        assert_eq!(
            active_members([e], NOW)[0].backend_identities,
            [
                hashed_identity(user, "custom-id"),
                "@a:example.org:DEV".to_owned()
            ]
        );
    }

    /// GIVEN two devices of one user with the same membership ID WHEN read
    /// THEN neither keeps the shared hashed identity, because it cannot say
    /// which device it is; each keeps its own `{user}:{device}` identity.
    #[test]
    fn given_shared_membership_id_then_no_hashed_identity() {
        let both = json!([
            { "type": "livekit", "url": "wss://sfu.example.org", "livekit_service_url": SERVICE }
        ]);
        let events = ["DEV", "DEV2"].map(|device| {
            let mut e = good("@a:example.org", device, NOW);
            e.content["membershipID"] = json!("shared");
            e.content["foci_preferred"] = both.clone();
            e
        });
        let members = active_members(events, NOW);
        assert_eq!(members.len(), 2);
        for member in &members {
            assert_eq!(
                member.backend_identities,
                [format!("@a:example.org:{}", member.device_id)]
            );
        }

        // Only `url`: nothing is left, so the member has no backend identity.
        let only_url = json!([{ "type": "livekit", "url": "wss://sfu.example.org" }]);
        let events = ["DEV", "DEV2"].map(|device| {
            let mut e = good("@a:example.org", device, NOW);
            e.content["membershipID"] = json!("shared");
            e.content["foci_preferred"] = only_url.clone();
            e
        });
        assert!(
            active_members(events, NOW)
                .iter()
                .all(|member| member.backend_identities.is_empty())
        );
    }

    /// GIVEN a wrong application, scope, call id, device id or lifetime WHEN read THEN the event is skipped.
    #[test]
    fn given_wrong_fields_then_skipped() {
        for (field, value) in [
            ("application", json!("m.other")),
            ("scope", json!("m.user")),
            ("call_id", json!("abc")),
            ("device_id", json!("OTHER")),
            ("expires", json!(0)),
            ("expires", json!(MAX_EXPIRES_MS + 1)),
            ("expires", json!("14400000")),
            ("foci_preferred", json!([])),
            ("foci_preferred", json!([{"type": "other"}])),
            ("foci_preferred", json!("x")),
        ] {
            let mut e = good("@a:example.org", "DEV", NOW);
            e.content[field] = value.clone();
            assert!(active_members([e], NOW).is_empty(), "{field}={value}");
        }
    }

    /// GIVEN a membershipID with controls or a huge size WHEN read THEN the derived identity is used.
    #[test]
    fn given_bad_membership_id_then_derived_identity() {
        for bad in [
            json!("a\nb"),
            json!("x".repeat(MAX_IDENTITY_BYTES + 1)),
            json!(5),
        ] {
            let mut e = good("@a:example.org", "DEV", NOW);
            e.content["membershipID"] = bad;
            let members = active_members([e], NOW);
            assert_eq!(members[0].identity, "@a:example.org:DEV");
        }
        let mut e = good("@a:example.org", "DEV", NOW);
        e.content["membershipID"] = json!("custom-id");
        assert_eq!(active_members([e], NOW)[0].identity, "custom-id");
    }

    /// GIVEN two events for one device WHEN read THEN the newer join wins; created_ts lowers the join time.
    #[test]
    fn given_duplicate_device_then_newest_join_kept() {
        let old = good("@a:example.org", "DEV", NOW - 5000);
        let new = good("@a:example.org", "DEV", NOW - 1000);
        let members = active_members([old, new], NOW);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].joined_ts, NOW - 1000);

        let mut refreshed = good("@a:example.org", "DEV", NOW - 100);
        refreshed.content["created_ts"] = json!(NOW - 9000);
        assert_eq!(active_members([refreshed], NOW)[0].joined_ts, NOW - 9000);
    }

    /// GIVEN a created_ts after the server timestamp WHEN read THEN the server timestamp is the join time.
    #[test]
    fn given_future_created_ts_then_server_time_used() {
        let mut e = good("@a:example.org", "DEV", NOW - 100);
        e.content["created_ts"] = json!(NOW + 99_999);
        assert_eq!(active_members([e], NOW)[0].joined_ts, NOW - 100);
    }

    /// GIVEN more members than the cap WHEN read THEN the oldest joins are kept, in order.
    #[test]
    fn given_too_many_members_then_capped_oldest_first() {
        let events: Vec<_> = (0..MAX_CALL_MEMBERS + 5)
            .map(|i| {
                good(
                    &format!("@u{i}:example.org"),
                    "DEV",
                    NOW - 10_000 + i as u64,
                )
            })
            .collect();
        let members = active_members(events, NOW);
        assert_eq!(members.len(), MAX_CALL_MEMBERS);
        assert_eq!(members[0].identity, "@u0:example.org:DEV");
        assert!(members.windows(2).all(|w| w[0].joined_ts <= w[1].joined_ts));
    }

    /// GIVEN more events than the examine limit WHEN read THEN later events are not examined.
    #[test]
    fn given_too_many_events_then_later_ones_ignored() {
        let mut events: Vec<_> = (0..MAX_MEMBER_EVENTS)
            .map(|_| event("@x:example.org", "DEV", NOW, json!("junk")))
            .collect();
        events.push(good("@a:example.org", "DEV", NOW));
        assert!(active_members(events, NOW).is_empty());
    }

    /// GIVEN non-object contents WHEN read THEN nothing panics and nothing is kept.
    #[test]
    fn given_non_object_content_then_skipped() {
        for content in [json!(null), json!([1]), json!("x"), json!(1)] {
            assert!(active_members([event("@a:example.org", "DEV", NOW, content)], NOW).is_empty());
        }
    }
}
