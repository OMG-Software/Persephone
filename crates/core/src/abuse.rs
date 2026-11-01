//! Account ignore state and explicit reports to the account's homeserver.
//!
//! Ignore state uses the SDK's encrypted state store. Successful writes are
//! reflected locally before returning because SDK account-data writes wait
//! for sync to update their local copy. The session serializes these writes.

use std::collections::BTreeSet;

use matrix_sdk::{
    Client, RoomState,
    ruma::{
        EventId, OwnedUserId, RoomId, UserId,
        events::{
            GlobalAccountDataEventType,
            ignored_user_list::{IgnoredUser, IgnoredUserListEventContent},
        },
        serde::Raw,
    },
};
use matrix_sdk_base::store::StateChanges;
use persephone_contracts::IgnoredUsers;
use persephone_core_protocol::{ErrorCode, MAX_EVENT_ID_BYTES, MAX_ROOM_ID_BYTES};

pub const MAX_IGNORED_USERS: usize = 100;
pub const MAX_REASON_BYTES: usize = 1000;
const MAX_ACCOUNT_DATA_BYTES: usize = 256 * 1024;

async fn ignored_content(client: &Client) -> Result<IgnoredUserListEventContent, ErrorCode> {
    let raw = client
        .account()
        .account_data::<IgnoredUserListEventContent>()
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    match raw {
        Some(raw) if raw.json().get().len() > MAX_ACCOUNT_DATA_BYTES => {
            Err(ErrorCode::LimitExceeded)
        }
        Some(raw) => raw.deserialize().map_err(|_| ErrorCode::Unavailable),
        None => Ok(IgnoredUserListEventContent::default()),
    }
}

/// The complete local ignore set. Never use the truncated display list to
/// decide whether content or a notification may be shown. This reads no HTTP.
pub async fn ignored_set(client: &Client) -> Result<BTreeSet<OwnedUserId>, ErrorCode> {
    Ok(ignored_content(client)
        .await?
        .ignored_users
        .into_keys()
        .collect())
}

pub async fn ignored_users(client: &Client) -> Result<IgnoredUsers, ErrorCode> {
    let users = ignored_set(client).await?;
    Ok(IgnoredUsers {
        truncated: users.len() > MAX_IGNORED_USERS,
        users: users
            .into_iter()
            .take(MAX_IGNORED_USERS)
            .map(String::from)
            .collect(),
    })
}

fn checked_user(raw: &str) -> Result<&UserId, ErrorCode> {
    if raw.len() > 255 || raw.chars().any(char::is_control) {
        return Err(ErrorCode::InvalidArgument);
    }
    <&UserId>::try_from(raw).map_err(|_| ErrorCode::InvalidArgument)
}

fn failure(error: matrix_sdk::Error) -> ErrorCode {
    if error.as_client_api_error().is_some() {
        ErrorCode::Rejected
    } else {
        ErrorCode::Unavailable
    }
}

/// Add or remove a user. The caller must serialize account ignore writes.
pub async fn set_ignored(client: &Client, user_id: &str, ignored: bool) -> Result<(), ErrorCode> {
    let user = checked_user(user_id)?;
    let own = client.user_id().ok_or(ErrorCode::Unavailable)?;
    if user == own {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut content = ignored_content(client).await?;
    if ignored
        && !content.ignored_users.contains_key(user)
        && content.ignored_users.len() >= MAX_IGNORED_USERS
    {
        return Err(ErrorCode::LimitExceeded);
    }
    if ignored {
        content
            .ignored_users
            .insert(user.to_owned(), IgnoredUser::new());
    } else {
        content.ignored_users.remove(user);
    }
    // Send and persist the same snapshot. Calling ignore_user here would
    // read a second snapshot, which can differ if a sync completes between.
    client
        .account()
        .set_account_data(content.clone())
        .await
        .map_err(failure)?;
    let raw = Raw::new(&serde_json::json!({
        "type": "m.ignored_user_list", "content": content,
    }))
    .map_err(|_| ErrorCode::Internal)?;
    let mut changes = StateChanges::default();
    changes.account_data.insert(
        GlobalAccountDataEventType::IgnoredUserList,
        raw.cast_unchecked(),
    );
    client
        .state_store()
        .save_changes(&changes)
        .await
        .map_err(|_| ErrorCode::Unavailable)
}

fn checked_reason(reason: &str) -> Result<(), ErrorCode> {
    if reason.trim().is_empty()
        || reason.len() > MAX_REASON_BYTES
        || reason
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}

/// Send only an event reference and the user's reason. The caller obtains
/// explicit confirmation before this request discloses them to the server.
pub async fn report(
    client: &Client,
    room_id: &str,
    event_id: &str,
    reason: &str,
) -> Result<(), ErrorCode> {
    checked_reason(reason)?;
    if room_id.len() > MAX_ROOM_ID_BYTES || event_id.len() > MAX_EVENT_ID_BYTES {
        return Err(ErrorCode::InvalidArgument);
    }
    let room_id = RoomId::parse(room_id).map_err(|_| ErrorCode::InvalidArgument)?;
    let event_id = EventId::parse(event_id).map_err(|_| ErrorCode::InvalidArgument)?;
    let room = client
        .get_room(&room_id)
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)?;
    room.report_content(event_id, Some(reason.to_owned()))
        .await
        .map_err(failure)?;
    Ok(())
}

/// A synthetic SDK client with local room state. No sync or public server.
#[cfg(test)]
pub(crate) async fn test_client(url: &str, rooms: &[(&str, RoomState)]) -> Client {
    use matrix_sdk::{
        SessionMeta, SessionTokens,
        authentication::matrix::MatrixSession,
        config::RequestConfig,
        cross_process_lock::CrossProcessLockConfig,
        ruma::{device_id, user_id},
    };
    use matrix_sdk_base::{
        RoomInfo,
        store::{MemoryStore, StateStore, StoreConfig},
    };
    let state = MemoryStore::new();
    let mut changes = StateChanges::default();
    for (id, membership) in rooms {
        changes.add_room(RoomInfo::new(&RoomId::parse(*id).unwrap(), *membership));
    }
    state.save_changes(&changes).await.unwrap();
    let client = Client::builder()
        .homeserver_url(url)
        .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
        .respect_login_well_known(false)
        .request_config(
            RequestConfig::new()
                .disable_retry()
                .timeout(std::time::Duration::from_secs(2)),
        )
        .store_config(
            StoreConfig::new(CrossProcessLockConfig::multi_process("abuse-test"))
                .state_store(state),
        )
        .build()
        .await
        .unwrap();
    client
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@me:example.org").to_owned(),
                device_id: device_id!("ME").to_owned(),
            },
            tokens: SessionTokens {
                access_token: "synthetic".into(),
                refresh_token: None,
            },
        })
        .await
        .unwrap();
    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;
    client
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_invalid_users_and_reasons_then_they_are_rejected() {
        for user in ["", "alice", "@x:s\n", &format!("@{}:s", "x".repeat(256))] {
            assert!(checked_user(user).is_err(), "{user:?}");
        }
        assert!(checked_user("@alice:example.org").is_ok());
        for reason in [
            "",
            " \n\t",
            "bad\0reason",
            &"x".repeat(MAX_REASON_BYTES + 1),
        ] {
            assert_eq!(checked_reason(reason), Err(ErrorCode::InvalidArgument));
        }
        assert_eq!(checked_reason("Unwanted message\nPlease review."), Ok(()));
    }

    #[tokio::test]
    async fn given_ignore_write_success_and_failure_then_only_success_changes_local_filter() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path_regex},
        };
        let server = MockServer::start().await;
        let client = test_client(&server.uri(), &[]).await;
        Mock::given(method("PUT"))
            .and(path_regex(".*/account_data/m.ignored_user_list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(2)
            .mount(&server)
            .await;
        assert_eq!(set_ignored(&client, "@bob:example.org", true).await, Ok(()));
        assert!(
            ignored_set(&client)
                .await
                .unwrap()
                .contains(matrix_sdk::ruma::user_id!("@bob:example.org"))
        );
        assert_eq!(
            set_ignored(&client, "@bob:example.org", false).await,
            Ok(())
        );
        assert!(ignored_set(&client).await.unwrap().is_empty());
        server.reset().await;
        Mock::given(method("PUT"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(serde_json::json!({"errcode":"M_FORBIDDEN","error":"refused"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            set_ignored(&client, "@bob:example.org", true).await,
            Err(ErrorCode::Rejected)
        );
        assert!(ignored_set(&client).await.unwrap().is_empty());
        assert_eq!(
            set_ignored(&client, "@me:example.org", true).await,
            Err(ErrorCode::InvalidArgument)
        );
    }

    #[tokio::test]
    async fn given_joined_room_then_report_sends_only_event_reference_and_reason() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{body_json, method, path_regex},
        };
        let server = MockServer::start().await;
        let client = test_client(&server.uri(), &[("!room:example.org", RoomState::Joined)]).await;
        Mock::given(method("POST"))
            .and(path_regex(".*/rooms/.*/report/.*"))
            .and(body_json(
                serde_json::json!({"reason":"Please review this message"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            report(
                &client,
                "!room:example.org",
                "$event",
                "Please review this message"
            )
            .await,
            Ok(())
        );
        assert_eq!(
            report(&client, "!missing:example.org", "$event", "reason").await,
            Err(ErrorCode::NotFound)
        );
    }

    #[tokio::test]
    async fn given_large_or_invalid_ignore_state_then_filter_is_complete_or_fails_closed() {
        let server = wiremock::MockServer::start().await;
        let client = test_client(&server.uri(), &[]).await;
        let before = server.received_requests().await.unwrap().len();
        let users: serde_json::Map<_, _> = (0..101)
            .map(|n| (format!("@user{n}:example.org"), serde_json::json!({})))
            .collect();
        let mut changes = StateChanges::default();
        changes.account_data.insert(GlobalAccountDataEventType::IgnoredUserList,
            Raw::new(&serde_json::json!({"type":"m.ignored_user_list", "content":{"ignored_users":users}})).unwrap().cast_unchecked());
        client.state_store().save_changes(&changes).await.unwrap();
        assert_eq!(ignored_set(&client).await.unwrap().len(), 101);
        let display = ignored_users(&client).await.unwrap();
        assert_eq!(display.users.len(), 100);
        assert!(display.truncated);
        assert_eq!(
            set_ignored(&client, "@new:example.org", true).await,
            Err(ErrorCode::LimitExceeded)
        );
        assert_eq!(server.received_requests().await.unwrap().len(), before);
        changes.account_data.insert(
            GlobalAccountDataEventType::IgnoredUserList,
            Raw::new(
                &serde_json::json!({"type":"m.ignored_user_list", "content":{"ignored_users":[]}}),
            )
            .unwrap()
            .cast_unchecked(),
        );
        client.state_store().save_changes(&changes).await.unwrap();
        assert_eq!(ignored_set(&client).await, Err(ErrorCode::Unavailable));
    }
}
