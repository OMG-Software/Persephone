//! Whether this account's homeserver can support a call (ADR-0043,
//! ADR-0056).
//!
//! A call needs an SFU transport and, for most networks, TURN servers. Both
//! answers are the homeserver's word and are filtered before use.
//!
//! The core reads the transports itself, not through the SDK's typed
//! request: `matrix-sdk` 0.19.1 refuses a whole answer when one entry has a
//! `url` and no `livekit_service_url`, the form a homeserver that issues
//! tokens itself advertises (MSC4195). The order is the stable MSC4143 path,
//! then the unstable path, then the `.well-known` `rtc_foci` member, each
//! next one only on 404 `M_UNRECOGNIZED`. The answers are capped while they
//! are read (`bounded`).

use super::bounded::{self, RequestError};
use super::livekit_entry;
use crate::discovery;
use matrix_sdk::ruma::api::{client::voip::get_turn_server_info, error::ErrorKind};
use persephone_core_protocol::ErrorCode;
use serde_json::{Map, Value, json};
use std::time::Duration;
use url::Url;
use zeroize::Zeroizing;

/// Most TURN URIs kept.
pub const MAX_TURN_URIS: usize = 8;

/// Longest TURN URI kept, in bytes.
pub const MAX_TURN_URI_BYTES: usize = 512;

/// Longest TURN user name or password kept, in bytes.
const MAX_TURN_SECRET_BYTES: usize = 1024;

/// TURN servers and their short-lived credentials.
pub struct TurnServers {
    pub uris: Vec<String>,
    pub username: Zeroizing<String>,
    pub password: Zeroizing<String>,
    pub ttl: Duration,
}

impl std::fmt::Debug for TurnServers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServers")
            .field("uris", &self.uris.len())
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

/// The homeserver could not be asked.
#[derive(Debug)]
pub enum ReadinessError {
    Http(matrix_sdk::HttpError),
    /// A transports or `.well-known` request failed, or the server refused
    /// it.
    Transports,
}

fn valid_turn_uri(uri: &str) -> bool {
    uri.len() <= MAX_TURN_URI_BYTES
        && (uri.starts_with("turn:") || uri.starts_with("turns:"))
        && uri.bytes().all(|b| b.is_ascii_graphic())
}

/// Keep the valid URIs of a TURN answer. `None` when nothing usable is left.
fn filter_turn(
    uris: Vec<String>,
    username: String,
    password: String,
    ttl: Duration,
) -> Option<TurnServers> {
    if username.len() > MAX_TURN_SECRET_BYTES || password.len() > MAX_TURN_SECRET_BYTES {
        return None;
    }
    let uris: Vec<String> = uris
        .into_iter()
        .filter(|u| valid_turn_uri(u))
        .take(MAX_TURN_URIS)
        .collect();
    (!uris.is_empty()).then(|| TurnServers {
        uris,
        username: Zeroizing::new(username),
        password: Zeroizing::new(password),
        ttl,
    })
}

/// The homeserver's TURN servers. `None` when it has none or does not
/// implement the endpoint.
pub async fn turn_servers(
    client: &matrix_sdk::Client,
) -> Result<Option<TurnServers>, ReadinessError> {
    match client.send(get_turn_server_info::v3::Request::new()).await {
        Ok(r) => Ok(filter_turn(r.uris, r.username, r.password, r.ttl)),
        Err(error) => {
            let missing = error.as_client_api_error().is_some_and(|e| {
                e.status_code.as_u16() == 404
                    || matches!(
                        e.error_kind(),
                        Some(ErrorKind::Unrecognized | ErrorKind::NotFound)
                    )
            });
            if missing {
                Ok(None)
            } else {
                Err(ReadinessError::Http(error))
            }
        }
    }
}

/// Most bytes of a transports answer read.
pub const MAX_TRANSPORTS_BYTES: usize = 16 * 1024;

/// Most entries of one transport list examined.
pub const MAX_TRANSPORT_ENTRIES: usize = 8;

/// The stable MSC4143 transports path.
const TRANSPORTS_STABLE: &str = "/_matrix/client/v1/rtc/transports";

/// The unstable MSC4143 transports path.
const TRANSPORTS_UNSTABLE: &str = "/_matrix/client/unstable/org.matrix.msc4143/rtc/transports";

/// How the token for an SFU is got (ADR-0056).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// An OpenID token to the JWT service at `livekit_service_url`, which
    /// answers `/sfu/get` (ADR-0043). The LiveKit identity is
    /// `{user}:{device}`.
    OpenId,
    /// The homeserver's MSC4195 `get_token` for the SFU at `url`. The
    /// LiveKit identity is hashed.
    Homeserver,
}

/// A LiveKit transport as the homeserver advertised it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SfuService {
    pub flow: Flow,
    /// The parsed URL, for requests and comparisons: the JWT service
    /// (`https`) for [`Flow::OpenId`], the SFU (`wss`) for
    /// [`Flow::Homeserver`].
    pub url: Url,
    /// The advertised string, unchanged. The membership and the `get_token`
    /// request carry this string: Element Call compares transports as
    /// strings, the homeserver compares `url` byte for byte, and `Url` adds
    /// `/` to an empty path.
    pub advertised: String,
    /// The advertised `server_name` of a [`Flow::Homeserver`] transport. It
    /// is always the own server's name.
    pub server_name: Option<String>,
}

impl SfuService {
    /// The `foci_preferred` entry of the own membership. The type is always
    /// `livekit`, because `matrix-js-sdk` ignores `m.livekit`, and only the
    /// OpenID form has `livekit_alias`, as Element Call 0.28.2 writes it.
    pub fn focus(&self, room_id: &str) -> Value {
        match self.flow {
            Flow::OpenId => json!({
                "type": "livekit",
                "livekit_service_url": self.advertised,
                "livekit_alias": room_id,
            }),
            Flow::Homeserver => {
                let mut focus = Map::new();
                focus.insert("type".to_owned(), json!("livekit"));
                focus.insert("url".to_owned(), json!(self.advertised));
                if let Some(name) = &self.server_name {
                    focus.insert("server_name".to_owned(), json!(name));
                }
                Value::Object(focus)
            }
        }
    }
}

/// What the homeserver said about an SFU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Discovery {
    /// The first usable transport, in the server's order.
    Offered(SfuService),
    /// It offers no transport.
    NoneOffered,
    /// It offers transports, but none that Persephone can use, or its answer
    /// cannot be read.
    Unsupported,
}

/// The usable transport of one list entry, or `None` to skip it.
///
/// * Type `livekit` with `livekit_service_url`, with or without `url`: the
///   OpenID flow. The URL must be `https`.
/// * Type `livekit` or `m.livekit` with `url` and no `livekit_service_url`:
///   the homeserver flow. The URL must be `wss`, and a `server_name` must be
///   `own_server`.
///
/// Loopback `http` and `ws` URLs pass only with the `dev-disposable-servers`
/// feature.
pub fn transport(entry: &Value, own_server: &str) -> Option<SfuService> {
    let entry_fields = livekit_entry(entry)?;
    if let Some((advertised, url)) = entry_fields.service {
        return Some(SfuService {
            flow: Flow::OpenId,
            url,
            advertised,
            server_name: None,
        });
    }
    let (advertised, url) = entry_fields.sfu?;
    let server_name = match entry.get("server_name") {
        None => None,
        Some(name) => {
            let name = name.as_str()?;
            if name != own_server {
                return None;
            }
            Some(name.to_owned())
        }
    };
    Some(SfuService {
        flow: Flow::Homeserver,
        url,
        advertised,
        server_name,
    })
}

/// The first usable transport of `list`. A missing or empty list offers
/// none; a list that is not an array, or that has no usable entry among the
/// first [`MAX_TRANSPORT_ENTRIES`], is unsupported.
fn read_list(list: Option<&Value>, own_server: &str) -> Discovery {
    let Some(list) = list else {
        return Discovery::NoneOffered;
    };
    let Some(entries) = list.as_array() else {
        return Discovery::Unsupported;
    };
    if entries.is_empty() {
        return Discovery::NoneOffered;
    }
    entries
        .iter()
        .take(MAX_TRANSPORT_ENTRIES)
        .find_map(|entry| transport(entry, own_server))
        .map_or(Discovery::Unsupported, Discovery::Offered)
}

/// Reads a transports answer whose list is the member `member`
/// (`transports` on the stable path, `rtc_transports` on the unstable one).
/// An answer over [`MAX_TRANSPORTS_BYTES`] or that is not a JSON object is
/// unsupported.
pub fn read_answer(body: &[u8], member: &str, own_server: &str) -> Discovery {
    if body.len() > MAX_TRANSPORTS_BYTES {
        return Discovery::Unsupported;
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(document)) => read_list(document.get(member), own_server),
        _ => Discovery::Unsupported,
    }
}

/// Reads the `rtc_foci` member of a `.well-known/matrix/client` document,
/// in its MSC4143 name or the `m.rtc_foci` alias. A document that cannot be
/// read offers none, as in the SDK: the file is optional.
pub fn read_well_known(body: &[u8], own_server: &str) -> Discovery {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(document)) => read_list(
            document
                .get("org.matrix.msc4143.rtc_foci")
                .or_else(|| document.get("m.rtc_foci")),
            own_server,
        ),
        _ => Discovery::NoneOffered,
    }
}

/// The homeserver's first usable LiveKit transport (ADR-0056).
pub async fn sfu_transport(client: &matrix_sdk::Client) -> Result<Discovery, ReadinessError> {
    let well_known = client
        .user_id()
        .and_then(|user| discovery::origin(user.server_name().as_str()))
        .and_then(|origin| origin.join(".well-known/matrix/client").ok());
    discover(client, well_known).await
}

/// [`sfu_transport`] with the `.well-known` URL given, so tests can name a
/// local one. `None` skips that step.
async fn discover(
    client: &matrix_sdk::Client,
    well_known: Option<Url>,
) -> Result<Discovery, ReadinessError> {
    let own_server = client
        .user_id()
        .map(|user| user.server_name().to_string())
        .unwrap_or_default();
    for (path, member) in [
        (TRANSPORTS_STABLE, "transports"),
        (TRANSPORTS_UNSTABLE, "rtc_transports"),
    ] {
        match bounded::send(
            client,
            reqwest::Method::GET,
            path,
            None,
            MAX_TRANSPORTS_BYTES,
            discovery::DISCOVERY_TIMEOUT,
        )
        .await
        {
            Ok(answer) if answer.is_success() => {
                return Ok(read_answer(&answer.body, member, &own_server));
            }
            Ok(answer) if answer.is_not_implemented() => {}
            // An answer that cannot be read is unsupported, as one over
            // the limit that `read_answer` would refuse.
            Err(RequestError::TooLarge(status)) if (200..300).contains(&status) => {
                return Ok(Discovery::Unsupported);
            }
            Ok(_) | Err(_) => return Err(ReadinessError::Transports),
        }
    }
    let Some(url) = well_known else {
        return Ok(Discovery::NoneOffered);
    };
    // The file is optional, so its absence or an unreadable one offers
    // none. A request that fails or a server error is not an answer.
    match discovery::fetch(client.http_client(), url).await {
        Ok((200, body)) => Ok(read_well_known(&body, &own_server)),
        Ok((404, _)) | Err(ErrorCode::Rejected) => Ok(Discovery::NoneOffered),
        Ok(_) | Err(_) => Err(ReadinessError::Transports),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::calls::MAX_URL_BYTES;
    use matrix_sdk::{
        Client, SessionMeta, SessionTokens, authentication::matrix::MatrixSession,
        config::RequestConfig, ruma::user_id,
    };
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    const TURN_PATH: &str = "/_matrix/client/v3/voip/turnServer";
    const RTC_PATH: &str = "/_matrix/client/unstable/org.matrix.msc4143/rtc/transports";

    /// A client signed in to `server` as `@alice:synthetic.test`, without retries.
    pub(in crate::calls) async fn signed_in(server: &MockServer) -> Client {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "versions": ["v1.11"] })),
            )
            .mount(server)
            .await;
        let client = Client::builder()
            .homeserver_url(server.uri())
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();
        client
            .restore_session(MatrixSession {
                meta: SessionMeta {
                    user_id: user_id!("@alice:synthetic.test").to_owned(),
                    device_id: "PERSEPHONE".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            })
            .await
            .unwrap();
        client
    }

    async fn turn_answer(server: &MockServer, status: u16, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(TURN_PATH))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    /// GIVEN TURN URIs with bad entries WHEN filtered THEN only turn: and turns: URIs within the limits stay.
    #[test]
    fn given_mixed_turn_uris_then_only_valid_kept() {
        let long = format!("turn:{}", "a".repeat(MAX_TURN_URI_BYTES));
        let uris = vec![
            "turn:turn.example.org:3478?transport=udp".to_owned(),
            "stun:stun.example.org".to_owned(),
            "https://evil.example.org".to_owned(),
            "turn:bad uri".to_owned(),
            long,
            "turns:turn.example.org:5349".to_owned(),
        ];
        let got = filter_turn(uris, "u".into(), "p".into(), Duration::from_secs(60)).unwrap();
        assert_eq!(
            got.uris,
            [
                "turn:turn.example.org:3478?transport=udp",
                "turns:turn.example.org:5349"
            ]
        );
        assert!(!format!("{got:?}").contains("\"p\""));
    }

    /// GIVEN no valid URIs, or oversized secrets WHEN filtered THEN there are no TURN servers.
    #[test]
    fn given_nothing_usable_then_no_turn() {
        let ttl = Duration::from_secs(60);
        assert!(filter_turn(vec![], "u".into(), "p".into(), ttl).is_none());
        assert!(filter_turn(vec!["stun:x".into()], "u".into(), "p".into(), ttl).is_none());
        let big = "x".repeat(MAX_TURN_SECRET_BYTES + 1);
        assert!(filter_turn(vec!["turn:x".into()], big, "p".into(), ttl).is_none());
    }

    /// GIVEN more than eight URIs WHEN filtered THEN eight are kept.
    #[test]
    fn given_many_turn_uris_then_capped() {
        let uris = (0..20).map(|i| format!("turn:h{i}.example.org")).collect();
        let got = filter_turn(uris, "u".into(), "p".into(), Duration::ZERO).unwrap();
        assert_eq!(got.uris.len(), MAX_TURN_URIS);
    }

    const OWN: &str = "synthetic.test";

    fn offered(discovery: Discovery) -> SfuService {
        match discovery {
            Discovery::Offered(service) => service,
            other => panic!("expected a transport, got {other:?}"),
        }
    }

    /// GIVEN each transport form WHEN read THEN `livekit_service_url` means
    /// the OpenID flow, with or without `url`, and `url` alone means the
    /// homeserver flow, in either type name.
    #[test]
    fn given_each_transport_form_then_its_flow() {
        let service = transport(
            &json!({ "type": "livekit", "livekit_service_url": "https://jwt.example.org" }),
            OWN,
        )
        .unwrap();
        assert_eq!(service.flow, Flow::OpenId);
        assert_eq!(service.advertised, "https://jwt.example.org");
        assert_eq!(service.url.as_str(), "https://jwt.example.org/");

        let both = json!({ "type": "livekit", "url": "wss://sfu.example.org",
                           "livekit_service_url": "https://jwt.example.org/x" });
        let service = transport(&both, OWN).unwrap();
        assert_eq!(service.flow, Flow::OpenId);
        assert_eq!(service.advertised, "https://jwt.example.org/x");

        for kind in ["livekit", "m.livekit"] {
            let service = transport(
                &json!({ "type": kind, "url": "wss://sfu.example.org:7443" }),
                OWN,
            )
            .unwrap();
            assert_eq!(service.flow, Flow::Homeserver);
            assert_eq!(service.advertised, "wss://sfu.example.org:7443");
            assert_eq!(service.server_name, None);
        }
        let named =
            json!({ "type": "livekit", "url": "wss://sfu.example.org", "server_name": OWN });
        assert_eq!(
            transport(&named, OWN).unwrap().server_name.as_deref(),
            Some(OWN)
        );
    }

    /// GIVEN entries Persephone cannot use WHEN read THEN each is skipped.
    #[test]
    fn given_unusable_entries_then_skipped() {
        let long = format!("wss://sfu.example.org/{}", "a".repeat(MAX_URL_BYTES));
        let mut bad = vec![
            json!("livekit"),
            json!({}),
            json!({ "type": "other", "url": "wss://sfu.example.org" }),
            json!({ "type": "livekit" }),
            json!({ "type": "livekit", "url": 5 }),
            json!({ "type": "livekit", "url": "not a url" }),
            json!({ "type": "livekit", "url": "https://sfu.example.org" }),
            json!({ "type": "livekit", "url": "wss://u:p@sfu.example.org" }),
            json!({ "type": "livekit", "url": long }),
            json!({ "type": "livekit", "url": "wss://sfu.example.org", "server_name": "other.test" }),
            json!({ "type": "livekit", "url": "wss://sfu.example.org", "server_name": 1 }),
            json!({ "type": "livekit", "livekit_service_url": "wss://sfu.example.org" }),
            json!({ "type": "livekit", "livekit_service_url": 5, "url": "wss://sfu.example.org" }),
            // matrix-js-sdk ignores m.livekit, so the OpenID form needs the
            // type it reads.
            json!({ "type": "m.livekit", "livekit_service_url": "https://jwt.example.org" }),
            // A `url` names the SFU, never a JWT service (ADR-0056 replaces
            // the ADR-0043 amendment that read it so).
            json!({ "type": "m.livekit", "url": "https://jwt.example.org" }),
        ];
        if !cfg!(feature = "dev-disposable-servers") {
            bad.push(json!({ "type": "livekit", "url": "ws://127.0.0.1:7880" }));
            bad.push(json!({ "type": "livekit", "livekit_service_url": "http://127.0.0.1:8080" }));
        }
        for entry in &bad {
            assert_eq!(transport(entry, OWN), None, "{entry}");
        }
    }

    /// GIVEN URLs at and over the limit WHEN read THEN only the one at the
    /// limit is kept.
    #[test]
    fn given_a_url_at_and_over_the_limit_then_only_the_one_at_the_limit_is_read() {
        let base = "wss://sfu.example.org/";
        let at = format!("{base}{}", "a".repeat(MAX_URL_BYTES - base.len()));
        let over = format!("{at}a");
        let entry = |url: &str| json!({ "type": "livekit", "url": url });
        assert_eq!(transport(&entry(&at), OWN).unwrap().advertised, at);
        assert_eq!(transport(&entry(&over), OWN), None);
    }

    /// GIVEN transport answers WHEN read THEN the first usable entry is
    /// taken in server order, an empty or missing list offers none, and an
    /// answer with nothing usable is unsupported.
    #[test]
    fn given_answers_then_offered_none_or_unsupported() {
        let answer =
            |value: serde_json::Value| read_answer(value.to_string().as_bytes(), "transports", OWN);
        let service = offered(answer(json!({ "transports": [
            { "type": "livekit", "url": "https://wrong.example.org" },
            { "type": "livekit", "url": "wss://first.example.org" },
            { "type": "livekit", "livekit_service_url": "https://second.example.org" },
        ] })));
        assert_eq!(service.advertised, "wss://first.example.org");

        assert_eq!(answer(json!({ "transports": [] })), Discovery::NoneOffered);
        assert_eq!(answer(json!({})), Discovery::NoneOffered);
        assert_eq!(
            answer(
                json!({ "rtc_transports": [{ "type": "livekit", "url": "wss://a.example.org" }] })
            ),
            Discovery::NoneOffered
        );
        assert_eq!(answer(json!({ "transports": {} })), Discovery::Unsupported);
        assert_eq!(
            answer(json!({ "transports": [{ "type": "other" }] })),
            Discovery::Unsupported
        );
        assert_eq!(answer(json!([])), Discovery::Unsupported);
        assert_eq!(
            read_answer(b"<html>", "transports", OWN),
            Discovery::Unsupported
        );
    }

    /// GIVEN a usable entry after the first eight, or an answer over the byte
    /// limit WHEN read THEN the answer is unsupported.
    #[test]
    fn given_too_many_entries_or_bytes_then_unsupported() {
        let mut list = vec![json!({ "type": "other" }); MAX_TRANSPORT_ENTRIES];
        list.push(json!({ "type": "livekit", "url": "wss://late.example.org" }));
        let body = json!({ "transports": list }).to_string();
        assert_eq!(
            read_answer(body.as_bytes(), "transports", OWN),
            Discovery::Unsupported
        );

        let good = json!({ "transports": [{ "type": "livekit", "url": "wss://sfu.example.org" }] })
            .to_string();
        let padded = format!("{good}{}", " ".repeat(MAX_TRANSPORTS_BYTES - good.len()));
        assert!(matches!(
            read_answer(padded.as_bytes(), "transports", OWN),
            Discovery::Offered(_)
        ));
        let over = format!("{padded} ");
        assert_eq!(
            read_answer(over.as_bytes(), "transports", OWN),
            Discovery::Unsupported
        );
    }

    /// GIVEN `.well-known` documents WHEN read THEN the MSC4143 name and the
    /// `m.rtc_foci` alias are both read, the MSC4143 name first, and an
    /// unreadable document offers none.
    #[test]
    fn given_well_known_documents_then_rtc_foci_read() {
        let read = |value: serde_json::Value| read_well_known(value.to_string().as_bytes(), OWN);
        let unstable = json!([{ "type": "livekit", "url": "wss://a.example.org" }]);
        let alias = json!([{ "type": "livekit", "url": "wss://b.example.org" }]);
        assert_eq!(
            offered(read(
                json!({ "org.matrix.msc4143.rtc_foci": unstable, "m.rtc_foci": alias })
            ))
            .advertised,
            "wss://a.example.org"
        );
        assert_eq!(
            offered(read(json!({ "m.rtc_foci": alias }))).advertised,
            "wss://b.example.org"
        );
        assert_eq!(read(json!({ "m.homeserver": {} })), Discovery::NoneOffered);
        assert_eq!(read_well_known(b"not json", OWN), Discovery::NoneOffered);
        assert_eq!(
            read(json!({ "m.rtc_foci": [{ "type": "other" }] })),
            Discovery::Unsupported
        );
    }

    /// GIVEN each flow WHEN the membership focus is made THEN the OpenID form
    /// has the service URL and the room alias, the homeserver form has the
    /// advertised `url` and `server_name` and no alias, and both are type
    /// `livekit`.
    #[test]
    fn given_each_flow_then_its_focus() {
        let room = "!room:synthetic.test";
        let openid = transport(
            &json!({ "type": "livekit", "livekit_service_url": "https://jwt.example.org" }),
            OWN,
        )
        .unwrap();
        assert_eq!(
            openid.focus(room),
            json!({ "type": "livekit", "livekit_service_url": "https://jwt.example.org",
                    "livekit_alias": room })
        );
        let bare = transport(
            &json!({ "type": "m.livekit", "url": "wss://sfu.example.org" }),
            OWN,
        )
        .unwrap();
        assert_eq!(
            bare.focus(room),
            json!({ "type": "livekit", "url": "wss://sfu.example.org" })
        );
        let named = transport(
            &json!({ "type": "livekit", "url": "wss://sfu.example.org", "server_name": OWN }),
            OWN,
        )
        .unwrap();
        assert_eq!(
            named.focus(room),
            json!({ "type": "livekit", "url": "wss://sfu.example.org", "server_name": OWN })
        );
    }

    /// GIVEN a homeserver with TURN servers WHEN asked THEN valid servers come back.
    #[tokio::test]
    async fn given_turn_answer_then_servers() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        turn_answer(
            &server,
            200,
            json!({ "username": "u", "password": "p", "ttl": 86400,
                    "uris": ["turn:turn.example.org", "ftp://bad"] }),
        )
        .await;
        let got = turn_servers(&client).await.unwrap().unwrap();
        assert_eq!(got.uris, ["turn:turn.example.org"]);
        assert_eq!(got.ttl, Duration::from_secs(86400));
    }

    /// GIVEN a homeserver without the endpoint or without URIs WHEN asked THEN there are no TURN servers.
    #[tokio::test]
    async fn given_missing_or_empty_turn_then_none() {
        for (status, body) in [
            (404, json!({ "errcode": "M_UNRECOGNIZED", "error": "no" })),
            (404, json!({ "errcode": "M_NOT_FOUND", "error": "no" })),
            (
                200,
                json!({ "username": "", "password": "", "ttl": 0, "uris": [] }),
            ),
        ] {
            let server = MockServer::start().await;
            let client = signed_in(&server).await;
            turn_answer(&server, status, body).await;
            assert!(turn_servers(&client).await.unwrap().is_none());
        }
    }

    /// GIVEN a server failure WHEN TURN is asked THEN the error is reported, not hidden as no TURN.
    #[tokio::test]
    async fn given_server_failure_then_error() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        turn_answer(
            &server,
            500,
            json!({ "errcode": "M_UNKNOWN", "error": "boom" }),
        )
        .await;
        assert!(turn_servers(&client).await.is_err());
    }

    const STABLE_PATH: &str = "/_matrix/client/v1/rtc/transports";

    async fn answer(server: &MockServer, at: &str, status: u16, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    fn unrecognized() -> serde_json::Value {
        json!({ "errcode": "M_UNRECOGNIZED", "error": "no" })
    }

    /// GIVEN a homeserver with the stable path WHEN asked THEN its
    /// `transports` list is used and the unstable path is not asked.
    #[tokio::test]
    async fn given_stable_path_then_used() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        answer(
            &server,
            STABLE_PATH,
            200,
            json!({ "transports": [{ "type": "livekit", "url": "wss://sfu.example.org" }] }),
        )
        .await;
        Mock::given(method("GET"))
            .and(path(RTC_PATH))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let service = offered(sfu_transport(&client).await.unwrap());
        assert_eq!(service.flow, Flow::Homeserver);
        assert_eq!(service.advertised, "wss://sfu.example.org");
    }

    /// GIVEN a homeserver with only the unstable path WHEN asked THEN its
    /// `rtc_transports` list is used, in either form.
    #[tokio::test]
    async fn given_only_unstable_path_then_used() {
        for (entry, flow) in [
            (
                json!({ "type": "livekit", "url": "wss://sfu.example.org:7443" }),
                Flow::Homeserver,
            ),
            (
                json!({ "type": "livekit", "livekit_service_url": "https://jwt.example.org" }),
                Flow::OpenId,
            ),
        ] {
            let server = MockServer::start().await;
            let client = signed_in(&server).await;
            answer(&server, STABLE_PATH, 404, unrecognized()).await;
            answer(&server, RTC_PATH, 200, json!({ "rtc_transports": [entry] })).await;
            assert_eq!(offered(sfu_transport(&client).await.unwrap()).flow, flow);
        }
    }

    /// GIVEN neither path, and a `.well-known` file WHEN asked THEN its
    /// `rtc_foci` member is used; without the file there is no transport.
    #[tokio::test]
    async fn given_neither_path_then_well_known() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        answer(&server, STABLE_PATH, 404, unrecognized()).await;
        answer(&server, RTC_PATH, 404, unrecognized()).await;
        answer(
            &server,
            "/.well-known/matrix/client",
            200,
            json!({ "org.matrix.msc4143.rtc_foci": [{ "type": "livekit", "url": "wss://wk.example.org" }] }),
        )
        .await;
        let well_known =
            Url::parse(&format!("{}/.well-known/matrix/client", server.uri())).unwrap();
        let service = offered(discover(&client, Some(well_known)).await.unwrap());
        assert_eq!(service.advertised, "wss://wk.example.org");

        let missing = Url::parse(&format!("{}/missing", server.uri())).unwrap();
        assert_eq!(
            discover(&client, Some(missing)).await.unwrap(),
            Discovery::NoneOffered
        );
        Mock::given(method("GET"))
            .and(path("/broken"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let broken = Url::parse(&format!("{}/broken", server.uri())).unwrap();
        assert!(
            discover(&client, Some(broken)).await.is_err(),
            "a server error is not an answer"
        );
        assert_eq!(
            discover(&client, None).await.unwrap(),
            Discovery::NoneOffered
        );
    }

    /// GIVEN a homeserver that answers with an empty object WHEN asked THEN
    /// it offers no transport.
    #[tokio::test]
    async fn given_empty_answer_then_none_offered() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        answer(&server, STABLE_PATH, 200, json!({})).await;
        assert_eq!(
            sfu_transport(&client).await.unwrap(),
            Discovery::NoneOffered
        );
    }

    /// GIVEN a server failure, or a 404 that is not `M_UNRECOGNIZED` WHEN
    /// asked THEN the error is reported and no later path is asked.
    #[tokio::test]
    async fn given_failure_then_error_without_fallback() {
        for (status, body) in [
            (500, json!({ "errcode": "M_UNKNOWN", "error": "boom" })),
            (404, json!({ "errcode": "M_NOT_FOUND", "error": "no" })),
            (401, json!({ "errcode": "M_UNKNOWN_TOKEN", "error": "no" })),
        ] {
            let server = MockServer::start().await;
            let client = signed_in(&server).await;
            answer(&server, STABLE_PATH, status, body).await;
            Mock::given(method("GET"))
                .and(path(RTC_PATH))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            assert!(sfu_transport(&client).await.is_err(), "{status}");
        }
    }

    /// GIVEN a transports answer over the limit WHEN asked THEN it is
    /// refused while it is read, as an answer Persephone cannot use.
    #[tokio::test]
    async fn given_an_oversized_answer_then_unsupported() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        Mock::given(method("GET"))
            .and(path(STABLE_PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_TRANSPORTS_BYTES + 1)),
            )
            .mount(&server)
            .await;
        assert_eq!(
            sfu_transport(&client).await.unwrap(),
            Discovery::Unsupported
        );
    }
}
