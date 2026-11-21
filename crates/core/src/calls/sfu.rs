//! The LiveKit token requests (ADR-0043, ADR-0056).
//!
//! A call needs a short-lived LiveKit access token for one SFU room. There
//! are two ways to get one:
//!
//! * The OpenID flow: the JWT service at the transport's URL gives one in
//!   exchange for an OpenID token from this account's homeserver
//!   (`POST {service}/sfu/get`).
//! * The homeserver flow (MSC4195): the homeserver itself gives one for the
//!   SFU it advertised (`POST /_matrix/client/v1/rtc/livekit/get_token`).
//!
//! Every answer is untrusted: it is size-capped, and its token (and for
//! `/sfu/get` its URL) is checked before the call worker sees it.

use super::bounded::{self, RequestError};
use super::{MAX_URL_BYTES, is_service_url};
use matrix_sdk::ruma::{DeviceId, RoomId, api::client::account::request_openid_token};
use serde_json::Map;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;
use zeroize::Zeroizing;

/// Most bytes read from a service answer.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024;

/// Longest JWT accepted, in bytes.
pub const MAX_JWT_BYTES: usize = 8192;

/// How long the whole request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The SFU address and the token for it.
pub struct SfuGrant {
    /// The `wss://` URL of the SFU.
    pub url: Url,
    pub jwt: Zeroizing<String>,
}

impl std::fmt::Debug for SfuGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SfuGrant")
            .field("url", &self.url.as_str())
            .finish_non_exhaustive()
    }
}

/// An OpenID token from the homeserver.
pub struct OpenIdToken {
    pub access_token: Zeroizing<String>,
    pub token_type: String,
    pub matrix_server_name: String,
    pub expires_in: Duration,
}

impl std::fmt::Debug for OpenIdToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenIdToken")
            .field("matrix_server_name", &self.matrix_server_name)
            .finish_non_exhaustive()
    }
}

impl From<request_openid_token::v3::Response> for OpenIdToken {
    fn from(response: request_openid_token::v3::Response) -> Self {
        Self {
            access_token: Zeroizing::new(response.access_token),
            token_type: response.token_type.as_str().to_owned(),
            matrix_server_name: response.matrix_server_name.to_string(),
            expires_in: response.expires_in,
        }
    }
}

/// The JSON body of the `/sfu/get` request.
pub fn request_body(room_id: &RoomId, device_id: &DeviceId, openid: &OpenIdToken) -> Value {
    json!({
        "room": room_id.as_str(),
        "openid_token": {
            "access_token": openid.access_token.as_str(),
            "token_type": openid.token_type,
            "matrix_server_name": openid.matrix_server_name,
            "expires_in": openid.expires_in.as_secs(),
        },
        "device_id": device_id.as_str(),
    })
}

/// Why a service answer was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    TooLarge,
    NotJson,
    BadUrl,
    BadJwt,
}

/// Check a service answer.
pub fn parse_grant(body: &[u8]) -> Result<SfuGrant, Refusal> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(Refusal::TooLarge);
    }
    let mut value: Value = serde_json::from_slice(body).map_err(|_| Refusal::NotJson)?;
    let object = value.as_object_mut().ok_or(Refusal::NotJson)?;

    let raw_url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or(Refusal::BadUrl)?;
    if raw_url.len() > MAX_URL_BYTES {
        return Err(Refusal::BadUrl);
    }
    let url = Url::parse(raw_url).map_err(|_| Refusal::BadUrl)?;
    if !is_service_url(&url, "wss", "ws") {
        return Err(Refusal::BadUrl);
    }

    Ok(SfuGrant {
        url,
        jwt: checked_jwt(object)?,
    })
}

/// The `jwt` member of an answer: three non-empty parts of visible ASCII, at
/// most [`MAX_JWT_BYTES`].
///
/// The string is moved out of the parsed answer into zeroized memory before
/// it is checked, so no ordinary copy of it is left, also when it is refused.
fn checked_jwt(object: &mut Map<String, Value>) -> Result<Zeroizing<String>, Refusal> {
    let jwt = match object.remove("jwt") {
        Some(Value::String(jwt)) => Zeroizing::new(jwt),
        _ => return Err(Refusal::BadJwt),
    };
    let parts: Vec<&str> = jwt.split('.').collect();
    if jwt.len() > MAX_JWT_BYTES
        || !jwt.bytes().all(|b| b.is_ascii_graphic())
        || parts.len() != 3
        || parts.iter().any(|p| p.is_empty())
    {
        return Err(Refusal::BadJwt);
    }
    Ok(jwt)
}

/// Checks a homeserver `get_token` answer (ADR-0056). Only `jwt` is read;
/// the SFU is the advertised `sfu`, never a URL from the answer.
pub fn parse_token(body: &[u8], sfu: &Url) -> Result<SfuGrant, Refusal> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(Refusal::TooLarge);
    }
    let mut value: Value = serde_json::from_slice(body).map_err(|_| Refusal::NotJson)?;
    let object = value.as_object_mut().ok_or(Refusal::NotJson)?;
    Ok(SfuGrant {
        url: sfu.clone(),
        jwt: checked_jwt(object)?,
    })
}

/// Why the request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SfuError {
    /// The service URL is not `https`, or the SFU URL is not `wss` (or a
    /// development loopback).
    InsecureServiceUrl,
    /// The request could not be made or finished.
    Network,
    /// The service answered with this non-success status.
    Status(u16),
    /// The answer was refused.
    Refused(Refusal),
}

/// The URL `{service}/sfu/get`, keeping the service URL's own path.
fn sfu_get_url(service_url: &Url) -> Option<Url> {
    let mut url = service_url.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["sfu", "get"]);
    Some(url)
}

/// Ask the service for a grant. `http` is the profile's client from
/// `homeserver::http_client`, so TLS 1.3 and the proxy apply.
pub async fn get_grant(
    http: &reqwest::Client,
    service_url: &Url,
    body: &Value,
) -> Result<SfuGrant, SfuError> {
    if !is_service_url(service_url, "https", "http") {
        return Err(SfuError::InsecureServiceUrl);
    }
    post(http, service_url, body).await
}

async fn post(
    http: &reqwest::Client,
    service_url: &Url,
    body: &Value,
) -> Result<SfuGrant, SfuError> {
    let url = sfu_get_url(service_url).ok_or(SfuError::InsecureServiceUrl)?;
    let payload = serde_json::to_vec(body).map_err(|_| SfuError::Network)?;
    let mut response = http
        .post(url)
        .timeout(REQUEST_TIMEOUT)
        .header("content-type", "application/json")
        .body(payload)
        .send()
        .await
        .map_err(|_| SfuError::Network)?;
    let status = response.status();
    if !status.is_success() {
        return Err(SfuError::Status(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
    {
        return Err(SfuError::Refused(Refusal::TooLarge));
    }
    let mut buffer = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(|_| SfuError::Network)? {
        if buffer.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(SfuError::Refused(Refusal::TooLarge));
        }
        buffer.extend_from_slice(&chunk);
    }
    parse_grant(&buffer).map_err(SfuError::Refused)
}

/// The stable MSC4195 token path.
const TOKEN_STABLE: &str = "/_matrix/client/v1/rtc/livekit/get_token";

/// The unstable MSC4195 token path, the only one Element Call 0.28.2 uses.
const TOKEN_UNSTABLE: &str = "/_matrix/client/unstable/io.element.msc4195/rtc/livekit/get_token";

/// What a homeserver token is for (ADR-0056).
pub struct TokenFor<'a> {
    /// The advertised SFU URL, parsed.
    pub sfu: &'a Url,
    /// The advertised SFU URL string, with no change: the homeserver
    /// compares it byte for byte.
    pub advertised: &'a str,
    /// The server name of the own user ID.
    pub server_name: &'a str,
    pub room_id: &'a RoomId,
    /// This device's `membershipID`.
    pub member_id: &'a str,
}

/// The MSC4195 `slot_id` of the call: the `m.call` application with the
/// empty `call_id` that Persephone writes.
pub const SLOT_ID: &str = "m.call#ROOM";

/// The JSON body of a `get_token` request.
pub fn token_body(to: &TokenFor<'_>) -> Value {
    json!({
        "url": to.advertised,
        "server_name": to.server_name,
        "room_id": to.room_id.as_str(),
        "slot_id": SLOT_ID,
        "member_id": to.member_id,
    })
}

/// Asks the own homeserver for a token for its SFU (ADR-0056). The request
/// goes through the SDK's HTTP client with the session's access token, so
/// TLS 1.3, the proxy and the no-redirect policy apply, and the answer is
/// capped at [`MAX_RESPONSE_BYTES`] while it is read. The stable path is
/// asked first; the unstable path only on 404 `M_UNRECOGNIZED`.
pub async fn get_homeserver_grant(
    client: &matrix_sdk::Client,
    to: TokenFor<'_>,
) -> Result<SfuGrant, SfuError> {
    if !is_service_url(to.sfu, "wss", "ws") {
        return Err(SfuError::InsecureServiceUrl);
    }
    let body = token_body(&to);
    let ask = |path| {
        bounded::send(
            client,
            reqwest::Method::POST,
            path,
            Some(&body),
            MAX_RESPONSE_BYTES,
            REQUEST_TIMEOUT,
        )
    };
    let mut answer = ask(TOKEN_STABLE).await;
    if answer
        .as_ref()
        .is_ok_and(bounded::Answer::is_not_implemented)
    {
        answer = ask(TOKEN_UNSTABLE).await;
    }
    let answer = answer.map_err(|error| match error {
        RequestError::TooLarge(_) => SfuError::Refused(Refusal::TooLarge),
        RequestError::Network | RequestError::NoSession => SfuError::Network,
    })?;
    if !answer.is_success() {
        return Err(SfuError::Status(answer.status));
    }
    parse_token(&answer.body, to.sfu).map_err(SfuError::Refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{device_id, room_id};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    const JWT: &str = "aaa.bbb.ccc";

    fn grant_body() -> Value {
        json!({ "url": "wss://sfu.example.org", "jwt": JWT })
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    /// GIVEN an OpenID token WHEN the body is built THEN it has the room, device and token fields.
    #[test]
    fn given_openid_token_then_request_body_has_expected_fields() {
        let openid = OpenIdToken {
            access_token: Zeroizing::new("tok".to_owned()),
            token_type: "Bearer".to_owned(),
            matrix_server_name: "example.org".to_owned(),
            expires_in: Duration::from_secs(3600),
        };
        let body = request_body(room_id!("!r:example.org"), device_id!("DEV"), &openid);
        assert_eq!(
            body,
            json!({
                "room": "!r:example.org",
                "openid_token": {
                    "access_token": "tok",
                    "token_type": "Bearer",
                    "matrix_server_name": "example.org",
                    "expires_in": 3600,
                },
                "device_id": "DEV",
            })
        );
        assert!(!format!("{openid:?}").contains("tok\""));
    }

    /// GIVEN a good answer WHEN parsed THEN the grant has a wss URL and the token, hidden from debug.
    #[test]
    fn given_good_answer_then_grant() {
        let grant = parse_grant(grant_body().to_string().as_bytes()).unwrap();
        assert_eq!(grant.url.scheme(), "wss");
        assert_eq!(grant.jwt.as_str(), JWT);
        assert!(!format!("{grant:?}").contains(JWT));
    }

    /// GIVEN an answer over the cap WHEN parsed THEN it is refused before parsing.
    #[test]
    fn given_oversized_answer_then_refused() {
        let body = vec![b' '; MAX_RESPONSE_BYTES + 1];
        assert_eq!(parse_grant(&body).unwrap_err(), Refusal::TooLarge);
    }

    /// GIVEN a URL that is not safe WHEN parsed THEN it is refused.
    #[test]
    fn given_unsafe_sfu_url_then_refused() {
        let mut urls = vec![
            "https://sfu.example.org",
            "wss://user:pw@sfu.example.org",
            "wss://",
            "not a url",
            "",
        ];
        if !cfg!(feature = "dev-disposable-servers") {
            urls.extend([
                "ws://sfu.example.org",
                "ws://127.0.0.1:7880",
                "ws://[::1]:7880",
            ]);
        }
        for bad in urls {
            let body = json!({ "url": bad, "jwt": JWT });
            assert_eq!(
                parse_grant(body.to_string().as_bytes()).unwrap_err(),
                Refusal::BadUrl,
                "{bad:?}"
            );
        }
        let long = format!("wss://sfu.example.org/{}", "a".repeat(MAX_URL_BYTES));
        let body = json!({ "url": long, "jwt": JWT });
        assert_eq!(
            parse_grant(body.to_string().as_bytes()).unwrap_err(),
            Refusal::BadUrl
        );
    }

    /// GIVEN loopback ws with the development feature WHEN parsed THEN it is accepted only for loopback.
    #[cfg(feature = "dev-disposable-servers")]
    #[test]
    fn given_dev_feature_then_ws_loopback_only() {
        let ok = json!({ "url": "ws://127.0.0.1:7880", "jwt": JWT });
        assert!(parse_grant(ok.to_string().as_bytes()).is_ok());
        let bad = json!({ "url": "ws://sfu.example.org", "jwt": JWT });
        assert!(parse_grant(bad.to_string().as_bytes()).is_err());
    }

    /// GIVEN malformed tokens WHEN parsed THEN they are refused.
    #[test]
    fn given_malformed_jwt_then_refused() {
        let too_long = format!("a.b.{}", "c".repeat(MAX_JWT_BYTES));
        for bad in [
            json!("aaa.bbb"),
            json!("aaa.bbb.ccc.ddd"),
            json!("aaa..ccc"),
            json!(".bbb.ccc"),
            json!("aaa.bbb.ccc "),
            json!("aaa.bb\u{e9}.ccc"),
            json!(""),
            json!(5),
            json!(too_long),
        ] {
            let body = json!({ "url": "wss://sfu.example.org", "jwt": bad });
            assert_eq!(
                parse_grant(body.to_string().as_bytes()).unwrap_err(),
                Refusal::BadJwt,
                "{bad}"
            );
        }
        let missing = json!({ "url": "wss://sfu.example.org" });
        assert_eq!(
            parse_grant(missing.to_string().as_bytes()).unwrap_err(),
            Refusal::BadJwt
        );
    }

    /// GIVEN non-JSON or non-object answers WHEN parsed THEN they are refused.
    #[test]
    fn given_not_json_then_refused() {
        assert_eq!(parse_grant(b"nope").unwrap_err(), Refusal::NotJson);
        assert_eq!(parse_grant(b"[1]").unwrap_err(), Refusal::NotJson);
    }

    /// GIVEN a plain http service WHEN a grant is requested THEN it is refused before any request.
    #[tokio::test]
    async fn given_http_service_then_refused_without_request() {
        if cfg!(feature = "dev-disposable-servers") {
            return;
        }
        let server = MockServer::start().await;
        let url = Url::parse(&server.uri()).unwrap();
        let r = get_grant(&client(), &url, &json!({})).await;
        assert_eq!(r.unwrap_err(), SfuError::InsecureServiceUrl);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// GIVEN a service with a path, with and without a trailing slash WHEN asked THEN /sfu/get is appended to that path.
    #[tokio::test]
    async fn given_service_path_then_sfu_get_is_appended() {
        for suffix in ["/livekit/jwt", "/livekit/jwt/"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/livekit/jwt/sfu/get"))
                .and(body_partial_json(json!({ "room": "!r:example.org" })))
                .respond_with(ResponseTemplate::new(200).set_body_json(grant_body()))
                .expect(1)
                .mount(&server)
                .await;
            let url = Url::parse(&format!("{}{suffix}", server.uri())).unwrap();
            let body = json!({ "room": "!r:example.org" });
            let grant = post(&client(), &url, &body).await.unwrap();
            assert_eq!(grant.jwt.as_str(), JWT, "{suffix}");
        }
    }

    /// GIVEN a service at the root WHEN asked THEN the path is /sfu/get.
    #[tokio::test]
    async fn given_root_service_then_plain_sfu_get() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sfu/get"))
            .respond_with(ResponseTemplate::new(200).set_body_json(grant_body()))
            .expect(1)
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();
        assert!(post(&client(), &url, &json!({})).await.is_ok());
    }

    /// GIVEN another SFU's service that answers with a redirect to a place
    /// that would give a grant WHEN the profile's client asks THEN the status
    /// is the answer and the redirect target is never requested.
    #[tokio::test]
    async fn given_a_redirect_then_it_is_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sfu/get"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "/elsewhere/sfu/get"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/elsewhere/sfu/get"))
            .respond_with(ResponseTemplate::new(200).set_body_json(grant_body()))
            .expect(0)
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();
        let http = crate::homeserver::http_client(None).unwrap();
        assert_eq!(
            post(&http, &url, &json!({})).await.unwrap_err(),
            SfuError::Status(302)
        );
    }

    /// GIVEN another SFU's service whose answer names a URL that is not `wss://`
    /// or has user information WHEN it is read THEN the grant is refused.
    #[tokio::test]
    async fn given_an_answer_with_a_bad_url_then_the_grant_is_refused() {
        for bad in [
            "https://sfu.example.org",
            "wss://u:p@sfu.example.org",
            "wss://",
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({ "url": bad, "jwt": JWT })),
                )
                .mount(&server)
                .await;
            let url = Url::parse(&server.uri()).unwrap();
            assert_eq!(
                post(&client(), &url, &json!({})).await.unwrap_err(),
                SfuError::Refused(Refusal::BadUrl),
                "{bad}"
            );
        }
    }

    /// GIVEN a server error WHEN asked THEN only the status is reported.
    #[tokio::test]
    async fn given_server_error_then_status_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("secret details"))
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();
        assert_eq!(
            post(&client(), &url, &json!({})).await.unwrap_err(),
            SfuError::Status(500)
        );
    }

    /// GIVEN an answer over the cap WHEN asked THEN it is refused as too large.
    #[tokio::test]
    async fn given_oversized_response_then_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_RESPONSE_BYTES + 1]),
            )
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();
        assert_eq!(
            post(&client(), &url, &json!({})).await.unwrap_err(),
            SfuError::Refused(Refusal::TooLarge)
        );
    }

    /// GIVEN an answer that fails the checks WHEN asked THEN the refusal is returned.
    #[tokio::test]
    async fn given_bad_grant_then_refusal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "url": "https://x.example.org", "jwt": JWT })),
            )
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();
        assert_eq!(
            post(&client(), &url, &json!({})).await.unwrap_err(),
            SfuError::Refused(Refusal::BadUrl)
        );
    }

    const STABLE_TOKEN: &str = "/_matrix/client/v1/rtc/livekit/get_token";
    const UNSTABLE_TOKEN: &str =
        "/_matrix/client/unstable/io.element.msc4195/rtc/livekit/get_token";
    const SFU: &str = "wss://sfu.example.org:7443";

    fn token_for<'a>(sfu: &'a Url, room: &'a RoomId) -> TokenFor<'a> {
        TokenFor {
            sfu,
            advertised: SFU,
            server_name: "synthetic.test",
            room_id: room,
            member_id: "@alice:synthetic.test:PERSEPHONE",
        }
    }

    fn expected_body() -> Value {
        json!({
            "url": SFU,
            "server_name": "synthetic.test",
            "room_id": "!r:synthetic.test",
            "slot_id": "m.call#ROOM",
            "member_id": "@alice:synthetic.test:PERSEPHONE",
        })
    }

    /// GIVEN a homeserver answer WHEN checked THEN only `jwt` is read, the
    /// SFU is the advertised one, and a bad or oversized answer is refused.
    #[test]
    fn given_token_answers_then_jwt_checked_and_sfu_kept() {
        let sfu = Url::parse(SFU).unwrap();
        let grant = parse_token(
            json!({ "jwt": JWT, "url": "wss://evil.example.org" })
                .to_string()
                .as_bytes(),
            &sfu,
        )
        .unwrap();
        assert_eq!(grant.url, sfu);
        assert_eq!(grant.jwt.as_str(), JWT);
        for (body, refusal) in [
            (json!({ "jwt": "a.b" }).to_string(), Refusal::BadJwt),
            (json!({}).to_string(), Refusal::BadJwt),
            ("[]".to_owned(), Refusal::NotJson),
            ("x".repeat(MAX_RESPONSE_BYTES + 1), Refusal::TooLarge),
        ] {
            assert_eq!(parse_token(body.as_bytes(), &sfu).unwrap_err(), refusal);
        }
    }

    /// GIVEN a homeserver with the stable token path WHEN a token is asked
    /// THEN the body has the advertised URL, own server, room, slot and
    /// member ID, no OpenID token, and the unstable path is not asked.
    #[tokio::test]
    async fn given_stable_token_path_then_used_with_the_body() {
        let server = MockServer::start().await;
        let client = crate::calls::readiness::tests::signed_in(&server).await;
        Mock::given(method("POST"))
            .and(path(STABLE_TOKEN))
            .and(wiremock::matchers::body_json(expected_body()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "jwt": JWT })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(UNSTABLE_TOKEN))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let sfu = Url::parse(SFU).unwrap();
        let room = room_id!("!r:synthetic.test");
        let grant = get_homeserver_grant(&client, token_for(&sfu, room))
            .await
            .unwrap();
        assert_eq!(grant.url, sfu);
        assert_eq!(grant.jwt.as_str(), JWT);
    }

    /// GIVEN a homeserver with only the unstable token path WHEN a token is
    /// asked THEN the unstable path gives it.
    #[tokio::test]
    async fn given_only_unstable_token_path_then_used() {
        let server = MockServer::start().await;
        let client = crate::calls::readiness::tests::signed_in(&server).await;
        Mock::given(method("POST"))
            .and(path(STABLE_TOKEN))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({ "errcode": "M_UNRECOGNIZED", "error": "no" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(UNSTABLE_TOKEN))
            .and(wiremock::matchers::body_json(expected_body()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "jwt": JWT })))
            .expect(1)
            .mount(&server)
            .await;
        let sfu = Url::parse(SFU).unwrap();
        let grant = get_homeserver_grant(&client, token_for(&sfu, room_id!("!r:synthetic.test")))
            .await
            .unwrap();
        assert_eq!(grant.jwt.as_str(), JWT);
    }

    /// GIVEN a refusal, a server failure or a bad answer WHEN a token is
    /// asked THEN the status or refusal is returned, and a refusal other than
    /// `M_UNRECOGNIZED` does not try the unstable path.
    #[tokio::test]
    async fn given_token_failures_then_errors() {
        for (status, body, expected) in [
            (
                403,
                json!({ "errcode": "M_FORBIDDEN", "error": "no" }),
                SfuError::Status(403),
            ),
            (
                500,
                json!({ "errcode": "M_UNKNOWN", "error": "no" }),
                SfuError::Status(500),
            ),
            (
                404,
                json!({ "errcode": "M_NOT_FOUND", "error": "no" }),
                SfuError::Status(404),
            ),
            (
                200,
                json!({ "jwt": "bad" }),
                SfuError::Refused(Refusal::BadJwt),
            ),
        ] {
            let server = MockServer::start().await;
            let client = crate::calls::readiness::tests::signed_in(&server).await;
            Mock::given(method("POST"))
                .and(path(STABLE_TOKEN))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            Mock::given(path(UNSTABLE_TOKEN))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "jwt": JWT })))
                .expect(0)
                .mount(&server)
                .await;
            let sfu = Url::parse(SFU).unwrap();
            let got = get_homeserver_grant(&client, token_for(&sfu, room_id!("!r:synthetic.test")))
                .await
                .unwrap_err();
            assert_eq!(got, expected, "{status}");
        }
    }

    /// GIVEN an SFU URL that is not `wss` WHEN a token is asked THEN it is
    /// refused before any request.
    #[tokio::test]
    async fn given_insecure_sfu_then_refused_without_request() {
        let server = MockServer::start().await;
        let client = crate::calls::readiness::tests::signed_in(&server).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "jwt": JWT })))
            .expect(0)
            .mount(&server)
            .await;
        for bad in ["https://sfu.example.org", "wss://u:p@sfu.example.org"] {
            let sfu = Url::parse(bad).unwrap();
            assert_eq!(
                get_homeserver_grant(&client, token_for(&sfu, room_id!("!r:synthetic.test")))
                    .await
                    .unwrap_err(),
                SfuError::InsecureServiceUrl
            );
        }
    }

    /// GIVEN a token answer over the limit WHEN a token is asked THEN it is
    /// refused while it is read.
    #[tokio::test]
    async fn given_an_oversized_token_answer_then_refused() {
        let server = MockServer::start().await;
        let client = crate::calls::readiness::tests::signed_in(&server).await;
        Mock::given(method("POST"))
            .and(path(STABLE_TOKEN))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_RESPONSE_BYTES + 1)),
            )
            .mount(&server)
            .await;
        let sfu = Url::parse(SFU).unwrap();
        assert_eq!(
            get_homeserver_grant(&client, token_for(&sfu, room_id!("!r:synthetic.test")))
                .await
                .unwrap_err(),
            SfuError::Refused(Refusal::TooLarge)
        );
    }
}
