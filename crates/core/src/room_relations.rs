//! Bounded reads for pin replacement relations only. Other SDK responses do
//! not use this limit. The caller owns the total deadline and session lease.
use matrix_sdk::{
    Client, Room,
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    ruma::{
        EventId,
        api::{
            Metadata, client::relations::get_relating_events_with_rel_type,
            path_builder::PathBuilder,
        },
        events::{AnySyncTimelineEvent, room::encrypted::OriginalSyncRoomEncryptedEvent},
        serde::Raw,
    },
};
use persephone_core_protocol::ErrorCode;
use serde::Deserialize;
use std::borrow::Cow;
use zeroize::Zeroizing;

pub(crate) const MAX_RELATION_BYTES: usize = 256 * 1024;
const MAX_RELATIONS: usize = 20;

#[derive(Deserialize)]
struct Page {
    chunk: Vec<Raw<AnySyncTimelineEvent>>,
    next_batch: Option<String>,
}

/// Read at most one complete replacement page, then decrypt sequentially
/// through the room SDK policy. An unknown access token permits one SDK
/// refresh and one bounded retry. Other authentication failures fail closed.
pub(crate) async fn replacements(
    client: &Client,
    room: &Room,
    event: &EventId,
) -> Result<Vec<TimelineEvent>, ErrorCode> {
    let versions = client
        .supported_versions()
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    let builder = get_relating_events_with_rel_type::v1::Request::PATH_BUILDER;
    if !builder.is_supported(&versions) {
        return Err(ErrorCode::Unavailable);
    }
    let relation = "m.replace";
    let args: [&dyn std::fmt::Display; 3] = [&room.room_id(), &event, &relation];
    let url = builder
        .make_endpoint_url(
            Cow::Borrowed(&versions),
            client.homeserver().as_str(),
            &args,
            "limit=20",
        )
        .map_err(|_| ErrorCode::Unavailable)?;
    let page = request_page(client, &url).await?;
    let mut events = Vec::with_capacity(page.chunk.len());
    for raw in page.chunk {
        let event = if raw
            .get_field::<String>("type")
            .map_err(|_| ErrorCode::Rejected)?
            .as_deref()
            == Some("m.room.encrypted")
        {
            let encrypted: Raw<OriginalSyncRoomEncryptedEvent> =
                Raw::from_json_string(raw.json().get().to_owned())
                    .map_err(|_| ErrorCode::Rejected)?;
            let decrypted = room
                .decrypt_event(&encrypted, None)
                .await
                .map_err(|_| ErrorCode::Unavailable)?;
            if matches!(decrypted.kind, TimelineEventKind::UnableToDecrypt { .. }) {
                return Err(ErrorCode::Unavailable);
            }
            decrypted
        } else {
            raw.deserialize().map_err(|_| ErrorCode::Rejected)?;
            TimelineEvent::from_plaintext(raw)
        };
        events.push(event);
    }
    Ok(events)
}

/// Refresh only after a bounded Matrix unknown-token response. Rebuild the
/// authorization header from the SDK session, and retry at most once.
async fn request_page(client: &Client, url: &str) -> Result<Page, ErrorCode> {
    for attempt in 0..2 {
        let token = Zeroizing::new(client.access_token().ok_or(ErrorCode::Locked)?);
        let response = client
            .http_client()
            .get(url)
            .bearer_auth(token.as_str())
            .send()
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
        let status = response.status();
        let body = read_body(response).await?;
        if status.is_success() {
            return parse_page(&body);
        }
        if attempt == 0 && status.as_u16() == 401 {
            #[derive(Deserialize)]
            struct MatrixError {
                errcode: String,
            }
            if serde_json::from_slice::<MatrixError>(&body)
                .is_ok_and(|error| error.errcode == "M_UNKNOWN_TOKEN")
            {
                client
                    .refresh_access_token()
                    .await
                    .map_err(|_| ErrorCode::Unavailable)?;
                continue;
            }
        }
        return Err(ErrorCode::Unavailable);
    }
    Err(ErrorCode::Unavailable)
}

/// Check body size before extending the buffer, including error responses.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, ErrorCode> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RELATION_BYTES as u64)
    {
        return Err(ErrorCode::LimitExceeded);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ErrorCode::Unavailable)? {
        if chunk.len() > MAX_RELATION_BYTES.saturating_sub(body.len()) {
            return Err(ErrorCode::LimitExceeded);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_page(body: &[u8]) -> Result<Page, ErrorCode> {
    let page: Page = serde_json::from_slice(body).map_err(|_| ErrorCode::Rejected)?;
    if page.chunk.len() > MAX_RELATIONS || page.next_batch.is_some() {
        return Err(ErrorCode::LimitExceeded);
    }
    Ok(page)
}
#[cfg(test)]
async fn read_page(response: reqwest::Response) -> Result<Page, ErrorCode> {
    let success = response.status().is_success();
    let body = read_body(response).await?;
    if !success {
        return Err(ErrorCode::Unavailable);
    }
    parse_page(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    #[tokio::test]
    async fn given_bounded_complete_relations_then_raw_events_are_returned() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"chunk":[{"type":"m.room.message","event_id":"$edit"}]}),
            ))
            .mount(&server)
            .await;
        let response = reqwest::Client::new()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        assert_eq!(read_page(response).await.unwrap().chunk.len(), 1);
    }

    #[tokio::test]
    async fn given_bad_status_malformed_oversize_or_incomplete_then_refuse_page() {
        for (template, expected) in [
            (ResponseTemplate::new(503).set_body_string("failure"), ErrorCode::Unavailable),
            (ResponseTemplate::new(200).set_body_string("invalid json"), ErrorCode::Rejected),
            (ResponseTemplate::new(200).set_body_string("x".repeat(MAX_RELATION_BYTES + 1)), ErrorCode::LimitExceeded),
            (ResponseTemplate::new(200).set_body_json(serde_json::json!({"chunk":[],"next_batch":"more"})), ErrorCode::LimitExceeded),
            // These encrypted placeholders cannot be decrypted. Count rejection
            // must happen while the page is still raw, without any room/SDK.
            (ResponseTemplate::new(200).set_body_json(serde_json::json!({"chunk":vec![serde_json::json!({"type":"m.room.encrypted"}); MAX_RELATIONS+1]})), ErrorCode::LimitExceeded),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(template).mount(&server).await;
            let response = reqwest::Client::new().get(server.uri()).send().await.unwrap();
            assert!(matches!(read_page(response).await, Err(error) if error == expected));
        }
    }

    #[tokio::test]
    async fn given_unknown_length_chunked_oversize_then_abort_before_full_body_is_sent() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sent = Arc::new(AtomicUsize::new(0));
        let observed = sent.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let mut used = 0;
            while !request[..used].windows(4).any(|part| part == b"\r\n\r\n") {
                assert!(used < request.len(), "test request headers must be bounded");
                let read = stream.read(&mut request[used..]).await.unwrap();
                assert!(read > 0, "the test client must finish its request headers");
                used += read;
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..32 {
                if stream.write_all(b"10000\r\n").await.is_err()
                    || stream.write_all(&chunk).await.is_err()
                    || stream.write_all(b"\r\n").await.is_err()
                {
                    break;
                }
                observed.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/relations"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.content_length(), None);
        assert!(matches!(
            read_page(response).await,
            Err(ErrorCode::LimitExceeded)
        ));
        assert!(
            sent.load(Ordering::SeqCst) < 32,
            "must stop before consuming the full response"
        );
        server.abort();
        let _ = server.await;
    }
    async fn refresh_client(server: &MockServer) -> Client {
        // SDK token refresh uses send_inner, which selects the refresh
        // endpoint from the server's advertised Matrix versions.
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/_matrix/client/versions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"versions":["v1.19"]})),
            )
            .mount(server)
            .await;
        use matrix_sdk::{
            SessionMeta, SessionTokens, authentication::matrix::MatrixSession,
            config::RequestConfig, ruma::user_id,
        };
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
                    device_id: "REFRESH_TEST".into(),
                },
                tokens: SessionTokens {
                    access_token: "expired-token".into(),
                    refresh_token: Some("synthetic-refresh-token".into()),
                },
            })
            .await
            .unwrap();
        client
    }

    async fn refresh_endpoint(server: &MockServer, times: u64) {
        Mock::given(method("POST"))
            .and(wiremock::matchers::path("/_matrix/client/v3/refresh"))
            .and(wiremock::matchers::body_json(serde_json::json!({"refresh_token":"synthetic-refresh-token"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token":"new-token","refresh_token":"next-refresh-token"})))
            .expect(times).mount(server).await;
    }

    async fn authorized_response(
        server: &MockServer,
        token: &str,
        response: ResponseTemplate,
        times: u64,
    ) {
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/relations"))
            .and(wiremock::matchers::header(
                "authorization",
                format!("Bearer {token}"),
            ))
            .respond_with(response)
            .expect(times)
            .mount(server)
            .await;
    }

    fn unknown_token() -> ResponseTemplate {
        ResponseTemplate::new(401)
            .set_body_json(serde_json::json!({"errcode":"M_UNKNOWN_TOKEN","error":"expired"}))
    }

    #[tokio::test]
    async fn given_expired_token_then_sdk_refreshes_once_and_new_token_retries_relations() {
        let server = MockServer::start().await;
        let client = refresh_client(&server).await;
        authorized_response(&server, "expired-token", unknown_token(), 1).await;
        refresh_endpoint(&server, 1).await;
        authorized_response(
            &server,
            "new-token",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"chunk":[]})),
            1,
        )
        .await;
        assert!(
            request_page(&client, &format!("{}/relations", server.uri()))
                .await
                .unwrap()
                .chunk
                .is_empty()
        );
        assert_eq!(client.access_token().as_deref(), Some("new-token"));
    }

    #[tokio::test]
    async fn given_repeated_unknown_token_then_only_one_refresh_and_two_requests_occur() {
        let server = MockServer::start().await;
        let client = refresh_client(&server).await;
        authorized_response(&server, "expired-token", unknown_token(), 1).await;
        refresh_endpoint(&server, 1).await;
        authorized_response(&server, "new-token", unknown_token(), 1).await;
        assert!(matches!(
            request_page(&client, &format!("{}/relations", server.uri())).await,
            Err(ErrorCode::Unavailable)
        ));
    }

    #[tokio::test]
    async fn given_oversized_401_then_no_refresh_or_retry_occurs() {
        let server = MockServer::start().await;
        let client = refresh_client(&server).await;
        authorized_response(
            &server,
            "expired-token",
            ResponseTemplate::new(401).set_body_string("x".repeat(MAX_RELATION_BYTES + 1)),
            1,
        )
        .await;
        refresh_endpoint(&server, 0).await;
        assert!(matches!(
            request_page(&client, &format!("{}/relations", server.uri())).await,
            Err(ErrorCode::LimitExceeded)
        ));
    }

    #[tokio::test]
    async fn given_oversized_retry_401_then_retry_body_is_also_bounded() {
        let server = MockServer::start().await;
        let client = refresh_client(&server).await;
        authorized_response(&server, "expired-token", unknown_token(), 1).await;
        refresh_endpoint(&server, 1).await;
        authorized_response(
            &server,
            "new-token",
            ResponseTemplate::new(401).set_body_string("x".repeat(MAX_RELATION_BYTES + 1)),
            1,
        )
        .await;
        assert!(matches!(
            request_page(&client, &format!("{}/relations", server.uri())).await,
            Err(ErrorCode::LimitExceeded)
        ));
    }

    #[tokio::test]
    async fn given_other_auth_error_or_server_error_then_sdk_does_not_refresh() {
        for status in [401, 503] {
            let server = MockServer::start().await;
            let client = refresh_client(&server).await;
            authorized_response(
                &server,
                "expired-token",
                ResponseTemplate::new(status)
                    .set_body_json(serde_json::json!({"errcode":"M_FORBIDDEN","error":"denied"})),
                1,
            )
            .await;
            refresh_endpoint(&server, 0).await;
            assert!(matches!(
                request_page(&client, &format!("{}/relations", server.uri())).await,
                Err(ErrorCode::Unavailable)
            ));
        }
    }
}
