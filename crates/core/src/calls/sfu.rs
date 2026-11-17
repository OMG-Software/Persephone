//! The LiveKit JWT service request (ADR-0043).
//!
//! A call needs a short-lived LiveKit access token for one SFU room. The
//! service at the transport's URL gives one in exchange for an OpenID token
//! from this account's homeserver (`POST {service}/sfu/get`). The answer is
//! untrusted: it is size-capped while read, and its URL and token are checked
//! before the call worker sees them.

use super::{MAX_URL_BYTES, is_service_url};
use matrix_sdk::ruma::{DeviceId, RoomId, api::client::account::request_openid_token};
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
    let value: Value = serde_json::from_slice(body).map_err(|_| Refusal::NotJson)?;
    let object = value.as_object().ok_or(Refusal::NotJson)?;

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

    let jwt = object
        .get("jwt")
        .and_then(Value::as_str)
        .ok_or(Refusal::BadJwt)?;
    let parts: Vec<&str> = jwt.split('.').collect();
    if jwt.len() > MAX_JWT_BYTES
        || !jwt.bytes().all(|b| b.is_ascii_graphic())
        || parts.len() != 3
        || parts.iter().any(|p| p.is_empty())
    {
        return Err(Refusal::BadJwt);
    }
    Ok(SfuGrant {
        url,
        jwt: Zeroizing::new(jwt.to_owned()),
    })
}

/// Why the request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SfuError {
    /// The service URL is not `https` (or a development loopback).
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
}
