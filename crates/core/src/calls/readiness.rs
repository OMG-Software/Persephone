//! Whether this account's homeserver can support a call (ADR-0043).
//!
//! A call needs an SFU transport (MSC4143 discovery, then the `.well-known`
//! fallback) and, for most networks, TURN servers. Both answers are the
//! homeserver's word and are filtered before use.

use super::{MAX_URL_BYTES, is_service_url};
use matrix_sdk::ruma::{
    api::client::{rtc::RtcTransport, voip::get_turn_server_info},
    api::error::ErrorKind,
};
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

/// A LiveKit service as the homeserver advertised it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SfuService {
    /// The parsed URL, for requests and comparisons.
    pub url: Url,
    /// The advertised string, unchanged. The membership carries this
    /// string, because Element Call compares transports as strings and
    /// `Url` adds `/` to an empty path.
    pub advertised: String,
}

/// The advertised URL string of a LiveKit transport, in either form.
///
/// The legacy form is `{"type":"livekit","livekit_service_url":U}`. The
/// newer form (Synapse 1.161 and later) is `{"type":"m.livekit","url":U}`.
/// A URL longer than [`MAX_URL_BYTES`] is refused before it is copied.
fn livekit_url(transport: &RtcTransport) -> Option<String> {
    match transport {
        RtcTransport::LiveKit(livekit) => bounded_copy(&livekit.service_url),
        other if other.transport_type() == "m.livekit" => other
            .data()
            .get("url")
            .and_then(serde_json::Value::as_str)
            .and_then(bounded_copy),
        _ => None,
    }
}

/// A copy of `url` when it is at most [`MAX_URL_BYTES`] long.
fn bounded_copy(url: &str) -> Option<String> {
    (url.len() <= MAX_URL_BYTES).then(|| url.to_owned())
}

/// The first LiveKit service URL that is safe to use, in server order.
fn first_livekit(transports: &[RtcTransport]) -> Option<SfuService> {
    transports.iter().find_map(|transport| {
        let advertised = livekit_url(transport)?;
        Url::parse(&advertised)
            .ok()
            .filter(|url| is_service_url(url, "https", "http"))
            .map(|url| SfuService { url, advertised })
    })
}

/// The homeserver's first usable LiveKit service. `None` when it offers
/// none.
pub async fn sfu_transport(
    client: &matrix_sdk::Client,
) -> Result<Option<SfuService>, ReadinessError> {
    let transports = client
        .discover_rtc_transports()
        .await
        .map_err(ReadinessError::Http)?;
    Ok(transports.as_deref().and_then(first_livekit))
}

/// What the homeserver offers for a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallReadiness {
    pub sfu: Option<Url>,
    pub turn: bool,
}

pub async fn readiness(client: &matrix_sdk::Client) -> Result<CallReadiness, ReadinessError> {
    let sfu = sfu_transport(client).await?.map(|service| service.url);
    let turn = turn_servers(client).await?.is_some();
    Ok(CallReadiness { sfu, turn })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
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

    /// GIVEN LiveKit transports WHEN the first usable one is picked THEN http and oversized ones are passed over.
    #[test]
    fn given_transports_then_first_https_livekit() {
        let long = format!("https://sfu.example.org/{}", "a".repeat(MAX_URL_BYTES));
        let mut transports = vec![
            RtcTransport::livekit("https://u:p@sfu.example.org".to_owned()),
            RtcTransport::livekit(long),
            RtcTransport::livekit("not a url".to_owned()),
            RtcTransport::new("other", serde_json::Map::new()).unwrap(),
            RtcTransport::livekit("https://good.example.org/jwt".to_owned()),
            RtcTransport::livekit("https://later.example.org".to_owned()),
        ];
        if !cfg!(feature = "dev-disposable-servers") {
            transports.insert(
                0,
                RtcTransport::livekit("http://sfu.example.org".to_owned()),
            );
            transports.insert(0, RtcTransport::livekit("http://127.0.0.1:8080".to_owned()));
        }
        assert_eq!(
            first_livekit(&transports).unwrap().url.as_str(),
            "https://good.example.org/jwt"
        );
        assert!(first_livekit(&[]).is_none());
    }

    /// GIVEN the newer and the legacy transport forms WHEN the first usable one is picked THEN both forms work, in server order, and the advertised string is kept.
    #[test]
    fn given_newer_transport_form_then_accepted() {
        let newer = |url: serde_json::Value| {
            let data = json!({ "url": url }).as_object().unwrap().clone();
            RtcTransport::new("m.livekit", data).unwrap()
        };
        let got = first_livekit(&[newer(json!("https://sfu.example.org"))]).unwrap();
        assert_eq!(got.advertised, "https://sfu.example.org");
        assert_eq!(got.url.as_str(), "https://sfu.example.org/");

        let mixed = [
            newer(json!("https://first.example.org/jwt")),
            RtcTransport::livekit("https://second.example.org".to_owned()),
        ];
        assert_eq!(
            first_livekit(&mixed).unwrap().url.as_str(),
            "https://first.example.org/jwt"
        );
        let mixed = [
            RtcTransport::livekit("https://legacy.example.org".to_owned()),
            newer(json!("https://newer.example.org")),
        ];
        assert_eq!(
            first_livekit(&mixed).unwrap().advertised,
            "https://legacy.example.org"
        );
    }

    /// GIVEN malformed newer entries WHEN the first usable one is picked THEN they are passed over.
    #[test]
    fn given_malformed_newer_entries_then_rejected() {
        let make = |data: serde_json::Value| {
            RtcTransport::new("m.livekit", data.as_object().unwrap().clone()).unwrap()
        };
        let bad = [
            make(json!({})),
            make(json!({ "url": 5 })),
            make(json!({ "url": "not a url" })),
            make(json!({ "url": "https://u:p@sfu.example.org" })),
            make(
                json!({ "url": format!("https://sfu.example.org/{}", "a".repeat(MAX_URL_BYTES)) }),
            ),
            make(json!({ "livekit_service_url": "https://sfu.example.org" })),
            RtcTransport::new("m.other", serde_json::Map::new()).unwrap(),
        ];
        assert!(first_livekit(&bad).is_none());
        let mut list = bad.to_vec();
        list.push(make(json!({ "url": "https://good.example.org" })));
        assert_eq!(
            first_livekit(&list).unwrap().advertised,
            "https://good.example.org"
        );
        if !cfg!(feature = "dev-disposable-servers") {
            assert!(first_livekit(&[make(json!({ "url": "http://sfu.example.org" }))]).is_none());
        }
    }

    /// GIVEN advertised URLs at and over the limit WHEN either form is read THEN only the one at the limit is kept.
    #[test]
    fn given_a_url_at_and_over_the_limit_then_only_the_one_at_the_limit_is_read() {
        let base = "https://sfu.example.org/";
        let at = format!("{base}{}", "a".repeat(MAX_URL_BYTES - base.len()));
        let over = format!("{at}a");
        let newer = |url: &str| {
            let data = json!({ "url": url });
            RtcTransport::new("m.livekit", data.as_object().unwrap().clone()).unwrap()
        };
        assert_eq!(
            livekit_url(&RtcTransport::livekit(at.clone())),
            Some(at.clone())
        );
        assert_eq!(livekit_url(&newer(&at)), Some(at.clone()));
        assert_eq!(livekit_url(&RtcTransport::livekit(over.clone())), None);
        assert_eq!(livekit_url(&newer(&over)), None);
    }

    /// GIVEN a service advertised as a bare origin, with or without a final
    /// slash WHEN it is picked THEN the advertised string is kept unchanged,
    /// though the parsed URL always ends in `/`.
    #[test]
    fn given_bare_origin_then_advertised_string_is_kept() {
        for advertised in ["https://sfu.example.org", "https://sfu.example.org/"] {
            let service = first_livekit(&[RtcTransport::livekit(advertised.to_owned())]).unwrap();
            assert_eq!(service.advertised, advertised);
            assert_eq!(service.url.as_str(), "https://sfu.example.org/");
        }
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

    /// GIVEN advertised transports WHEN asked THEN the first usable LiveKit URL comes back.
    #[tokio::test]
    async fn given_rtc_transports_then_sfu_url() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        Mock::given(method("GET"))
            .and(path(RTC_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rtc_transports": [
                    { "type": "livekit", "livekit_service_url": "http://plain.example.org" },
                    { "type": "livekit", "livekit_service_url": "https://sfu.example.org/jwt" }
                ]
            })))
            .mount(&server)
            .await;
        let service = sfu_transport(&client).await.unwrap().unwrap();
        assert_eq!(service.url.as_str(), "https://sfu.example.org/jwt");
        assert_eq!(service.advertised, "https://sfu.example.org/jwt");
        Mock::given(method("GET"))
            .and(path(TURN_PATH))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({ "errcode": "M_UNRECOGNIZED", "error": "no" })),
            )
            .mount(&server)
            .await;
        let ready = readiness(&client).await.unwrap();
        assert_eq!(ready.sfu.unwrap().as_str(), "https://sfu.example.org/jwt");
        assert!(!ready.turn);
    }
}
