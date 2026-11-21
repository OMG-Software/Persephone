//! Bounded client-server requests for the call service (ADR-0056).
//!
//! The SDK's `Client::send` reads a whole answer into memory before it is
//! decoded, so it cannot cap the call transport and token answers. These
//! requests go through the SDK's HTTP client instead (TLS 1.3, the profile
//! proxy, no redirects) with the session's access token, as
//! `room_relations` does. Each body, error bodies included, is capped while
//! it is read. An unknown access token permits one SDK refresh and one
//! retry; any other failure is returned.

use reqwest::Method;
use serde_json::Value;
use std::time::Duration;
use zeroize::Zeroizing;

/// A complete answer of at most the requested size.
pub struct Answer {
    pub status: u16,
    pub body: Zeroizing<Vec<u8>>,
}

impl Answer {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 404 `M_UNRECOGNIZED`: the server does not have the endpoint.
    pub fn is_not_implemented(&self) -> bool {
        self.status == 404 && errcode(&self.body).as_deref() == Some("M_UNRECOGNIZED")
    }
}

/// Why a request has no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The request could not be made or finished, or the token refresh failed.
    Network,
    /// The answer with this status was longer than the limit.
    TooLarge(u16),
    /// The session has no access token.
    NoSession,
}

fn errcode(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct MatrixError {
        errcode: String,
    }
    serde_json::from_slice::<MatrixError>(body)
        .ok()
        .map(|error| error.errcode)
}

/// Sends `method` to `path` on the session's homeserver, with `json` as
/// the body when given, and reads at most `max` bytes of the answer within
/// `timeout` for each attempt.
pub async fn send(
    client: &matrix_sdk::Client,
    method: Method,
    path: &str,
    json: Option<&Value>,
    max: usize,
    timeout: Duration,
) -> Result<Answer, RequestError> {
    let url = format!(
        "{}{path}",
        client.homeserver().as_str().trim_end_matches('/')
    );
    let payload = match json {
        Some(value) => Some(serde_json::to_vec(value).map_err(|_| RequestError::Network)?),
        None => None,
    };
    for attempt in 0..2 {
        let token = Zeroizing::new(client.access_token().ok_or(RequestError::NoSession)?);
        let mut request = client
            .http_client()
            .request(method.clone(), &url)
            .bearer_auth(token.as_str())
            .timeout(timeout);
        if let Some(payload) = &payload {
            request = request
                .header("content-type", "application/json")
                .body(payload.clone());
        }
        let response = request.send().await.map_err(|_| RequestError::Network)?;
        let answer = read(response, max).await?;
        if attempt == 0
            && answer.status == 401
            && errcode(&answer.body).as_deref() == Some("M_UNKNOWN_TOKEN")
        {
            client
                .refresh_access_token()
                .await
                .map_err(|_| RequestError::Network)?;
            continue;
        }
        return Ok(answer);
    }
    Err(RequestError::Network)
}

/// Reads at most `max` bytes, checking the declared length first and each
/// chunk before the buffer grows.
async fn read(mut response: reqwest::Response, max: usize) -> Result<Answer, RequestError> {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|size| size > max as u64)
    {
        return Err(RequestError::TooLarge(status));
    }
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(|_| RequestError::Network)? {
        if chunk.len() > max.saturating_sub(body.len()) {
            return Err(RequestError::TooLarge(status));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Answer { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::readiness::tests::signed_in;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    const AT: &str = "/_matrix/client/v1/test";
    const WAIT: Duration = Duration::from_secs(10);

    /// GIVEN an answer within the limit WHEN asked THEN it comes back whole,
    /// with the session's access token sent and the JSON body posted.
    #[tokio::test]
    async fn given_an_answer_within_the_limit_then_it_arrives_whole() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        Mock::given(method("POST"))
            .and(path(AT))
            .and(header("authorization", "Bearer token"))
            .and(wiremock::matchers::body_json(json!({ "a": 1 })))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(64)))
            .expect(1)
            .mount(&server)
            .await;
        let answer = send(
            &client,
            Method::POST,
            AT,
            Some(&json!({ "a": 1 })),
            64,
            WAIT,
        )
        .await
        .unwrap();
        assert!(answer.is_success());
        assert_eq!(answer.body.len(), 64);
    }

    /// GIVEN answers longer than the limit, also error answers WHEN asked
    /// THEN each is refused with its status.
    #[tokio::test]
    async fn given_an_answer_over_the_limit_then_it_is_refused() {
        for status in [200, 404, 500] {
            let server = MockServer::start().await;
            let client = signed_in(&server).await;
            Mock::given(method("GET"))
                .and(path(AT))
                .respond_with(ResponseTemplate::new(status).set_body_string("x".repeat(65)))
                .mount(&server)
                .await;
            assert_eq!(
                send(&client, Method::GET, AT, None, 64, WAIT).await.err(),
                Some(RequestError::TooLarge(status))
            );
        }
    }

    /// GIVEN error answers WHEN read THEN only 404 `M_UNRECOGNIZED` means the
    /// endpoint is missing.
    #[tokio::test]
    async fn given_error_answers_then_only_unrecognized_404_is_not_implemented() {
        for (status, body, missing) in [
            (404, json!({ "errcode": "M_UNRECOGNIZED" }), true),
            (404, json!({ "errcode": "M_NOT_FOUND" }), false),
            (400, json!({ "errcode": "M_UNRECOGNIZED" }), false),
            (404, json!("not an error"), false),
        ] {
            let server = MockServer::start().await;
            let client = signed_in(&server).await;
            Mock::given(method("GET"))
                .and(path(AT))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            let answer = send(&client, Method::GET, AT, None, 1024, WAIT)
                .await
                .unwrap();
            assert_eq!(answer.is_not_implemented(), missing, "{status}");
        }
    }

    /// GIVEN an unknown token and a session without a refresh token WHEN
    /// asked THEN the failed refresh is an error and nothing is retried.
    #[tokio::test]
    async fn given_unknown_token_without_refresh_then_error_and_no_retry() {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        Mock::given(method("GET"))
            .and(path(AT))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({ "errcode": "M_UNKNOWN_TOKEN" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            send(&client, Method::GET, AT, None, 1024, WAIT).await.err(),
            Some(RequestError::Network)
        );
    }
}
