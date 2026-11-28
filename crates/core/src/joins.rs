//! Joining a room with a long wait for the homeserver (owner decisions of 27
//! and 28 November 2026).
//!
//! * To join a room on another server, the homeserver gets and checks the
//!   room's state over federation. For a big room that can take minutes. So
//!   a join here waits up to [`JOIN_TIMEOUT`], not the 30 s of other
//!   requests. The SDK's joins use the client's 30 s, so this module sends
//!   the join request itself, with its own request configuration.
//! * A join keeps a session lease while it waits. Every [`JOIN_CHECK`] it
//!   asks `gone` whether the session locked, a sign-out waits, or the page
//!   closed. It then stops at once and is `locked`, so none of them waits for
//!   it. The homeserver can still finish the join, and the room then comes
//!   with sync.
//! * The SDK's own join also marks the room as joined in the store at once.
//!   For an invitation, it also records the inviter, so that it can import
//!   the room history the inviter shared (a room key bundle). Those steps
//!   are not public. So after the answer, a join here waits up to
//!   [`JOIN_LISTED_WAIT`] for sync to show the room as joined. An invitation
//!   joined here imports no shared history: that fails closed.
//!   `conversations::accept` therefore uses this join only for an invitation
//!   whose invite state shows no encryption.
//! * Directory joins and invitation answers run one at a time
//!   (`Core::joining`), because a join can keep its lease for minutes and
//!   sync needs the same leases.

use matrix_sdk::{
    Client, RoomState,
    config::RequestConfig,
    ruma::{
        OwnedRoomId, OwnedServerName, RoomId, api::client::membership::join_room_by_id_or_alias,
    },
};
use persephone_core_protocol::ErrorCode;
use std::time::Duration;

/// Longest wait for the homeserver to answer a join (owner decision,
/// 27 November 2026). See the module documentation.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest wait after the homeserver joined for sync to show the room as
/// joined, so that the page finds it in the room list. The join is done
/// either way.
const JOIN_LISTED_WAIT: Duration = Duration::from_secs(10);

/// How often a join checks whether the session or the page that asked for
/// it has gone, and whether sync shows the joined room.
const JOIN_CHECK: Duration = Duration::from_millis(100);

/// A refusal from the homeserver (a 4xx answer) is `rejected`. Anything
/// transient is `unavailable`: no answer at all, a server error (5xx), or a
/// rate limit (429), as for media (`media.rs`). A homeserver that is still
/// busy with a join of the room can answer 429.
pub fn request_error(error: Option<&matrix_sdk::ruma::api::error::Error>) -> ErrorCode {
    match error {
        Some(error) if error.status_code.is_client_error() && error.status_code.as_u16() != 429 => {
            ErrorCode::Rejected
        }
        _ => ErrorCode::Unavailable,
    }
}

/// Joins `room_id` through the servers `via`, or through the servers the
/// homeserver picks when `via` is empty, as for an invitation. Returns the
/// room the homeserver joined. The homeserver has [`JOIN_TIMEOUT`] to
/// answer; after that the join is `unavailable`. When `gone` is true, the
/// join stops at once and is `locked`.
pub async fn join(
    client: &Client,
    room_id: &RoomId,
    via: &[OwnedServerName],
    gone: &impl Fn() -> bool,
) -> Result<OwnedRoomId, ErrorCode> {
    join_within(client, room_id, via, JOIN_TIMEOUT, JOIN_LISTED_WAIT, gone).await
}

/// [`join`] with its two waits as arguments, for tests.
async fn join_within(
    client: &Client,
    room_id: &RoomId,
    via: &[OwnedServerName],
    answer: Duration,
    listed: Duration,
    gone: &impl Fn() -> bool,
) -> Result<OwnedRoomId, ErrorCode> {
    let mut request = join_room_by_id_or_alias::v3::Request::new(room_id.to_owned().into());
    request.via = via.to_owned();
    // This replaces the client's configuration for this request only, so it
    // also turns retries off again.
    let config = RequestConfig::new().disable_retry().timeout(answer);
    let sent = client.send(request).with_request_config(config);
    let response = until_gone(sent.into_future(), gone)
        .await
        .ok_or(ErrorCode::Locked)?
        .map_err(|error| request_error(error.as_client_api_error()))?;
    let _ = tokio::time::timeout(
        listed,
        until_gone(joined_here(client, &response.room_id), gone),
    )
    .await;
    Ok(response.room_id)
}

/// Runs `work` until it ends, or until `gone` is true at one of the checks
/// every [`JOIN_CHECK`]. Then `work` is dropped and the result is `None`.
async fn until_gone<T>(work: impl Future<Output = T>, gone: &impl Fn() -> bool) -> Option<T> {
    let watch = async {
        while !gone() {
            tokio::time::sleep(JOIN_CHECK).await;
        }
    };
    tokio::select! {
        biased;
        output = work => Some(output),
        () = watch => None,
    }
}

/// Ends when this client's store has `room_id` as a joined room.
async fn joined_here(client: &Client, room_id: &RoomId) {
    while !client
        .get_room(room_id)
        .is_some_and(|room| room.state() == RoomState::Joined)
    {
        tokio::time::sleep(JOIN_CHECK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::room_id;
    use serde_json::json;
    use std::time::Instant;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, path_regex},
    };

    /// GIVEN work that has not ended WHEN the asker goes THEN the work is
    /// dropped by the next check. GIVEN work that ended WHEN the asker has
    /// also gone THEN its output is kept: an answer that is there wins.
    #[tokio::test(start_paused = true)]
    async fn given_a_gone_asker_then_waiting_work_stops_at_the_next_check() {
        let gone = std::cell::Cell::new(false);
        let started = tokio::time::Instant::now();
        let stopped = until_gone(
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                gone.set(true);
                std::future::pending::<()>().await;
            },
            &|| gone.get(),
        )
        .await;
        assert_eq!(stopped, None);
        assert!(started.elapsed() <= Duration::from_millis(250) + JOIN_CHECK);
        assert_eq!(until_gone(async { 7 }, &|| true).await, Some(7));
    }

    /// A client of `server` with a session, whose requests time out after
    /// `timeout`, as the core's do after 30 s.
    async fn signed_in(server: &MockServer, timeout: Duration) -> Client {
        use matrix_sdk::{
            SessionMeta, SessionTokens, authentication::matrix::MatrixSession, ruma::user_id,
        };
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "versions": ["v1.11"] })),
            )
            .mount(server)
            .await;
        let client = Client::builder()
            .homeserver_url(server.uri())
            .request_config(RequestConfig::new().disable_retry().timeout(timeout))
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
                    access_token: "synthetic-token".into(),
                    refresh_token: None,
                },
            })
            .await
            .unwrap();
        client
    }

    /// GIVEN a client whose requests time out after 1 s WHEN the homeserver
    /// answers a join after 2 s THEN the join succeeds, because it has its
    /// own limit, and the reply comes after the listed wait although sync
    /// never shows the room. WHEN the join's own limit passes first THEN it
    /// is `unavailable`. WHEN the asker goes while the join waits THEN it
    /// stops at once and is `locked`. WHEN the homeserver answers with a
    /// rate limit THEN it is `unavailable`, and with a 403 `rejected`.
    #[tokio::test]
    async fn given_a_slow_homeserver_then_a_join_waits_for_its_own_limit() {
        let server = MockServer::builder().start().await;
        let client = signed_in(&server, Duration::from_secs(1)).await;
        let room = room_id!("!big:remote.test");
        let via = [OwnedServerName::try_from("remote.test").unwrap()];
        let answer = |status: u16, body: serde_json::Value, delay: Duration| {
            Mock::given(method("POST"))
                .and(path_regex("^/_matrix/client/v3/join/"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_json(body)
                        .set_delay(delay),
                )
                .up_to_n_times(1)
        };
        let join = async |answer: Duration, gone: &dyn Fn() -> bool| {
            join_within(
                &client,
                room,
                &via,
                answer,
                Duration::from_millis(300),
                &|| gone(),
            )
            .await
        };

        answer(200, json!({ "room_id": room }), Duration::from_secs(2))
            .mount(&server)
            .await;
        let started = Instant::now();
        let joined = join(Duration::from_secs(5), &|| false).await;
        assert_eq!(joined, Ok(room.to_owned()));
        assert!(started.elapsed() >= Duration::from_millis(2300));

        answer(200, json!({ "room_id": room }), Duration::from_secs(3))
            .mount(&server)
            .await;
        let started = Instant::now();
        let late = join(Duration::from_secs(1), &|| false).await;
        assert_eq!(late, Err(ErrorCode::Unavailable));
        assert!(started.elapsed() < Duration::from_secs(3));

        answer(200, json!({ "room_id": room }), Duration::from_secs(10))
            .mount(&server)
            .await;
        let started = Instant::now();
        let stopped = join(Duration::from_secs(30), &|| {
            started.elapsed() > Duration::from_millis(200)
        })
        .await;
        assert_eq!(stopped, Err(ErrorCode::Locked));
        assert!(started.elapsed() < Duration::from_secs(2));

        for (status, errcode, expected) in [
            (429, "M_UNKNOWN", ErrorCode::Unavailable),
            (403, "M_FORBIDDEN", ErrorCode::Rejected),
        ] {
            answer(
                status,
                json!({ "errcode": errcode, "error": "refused" }),
                Duration::ZERO,
            )
            .mount(&server)
            .await;
            let refused = join(Duration::from_secs(5), &|| false).await;
            assert_eq!(refused, Err(expected), "{status}");
        }
    }
}
