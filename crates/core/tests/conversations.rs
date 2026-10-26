//! Starting a conversation with the real SDK against a small mocked
//! homeserver: one direct-message room marked direct for Bob, encrypted or
//! not, with the members sync gives (the same list answers a member-list
//! request, for clients that load members lazily). Bob has no
//! cross-signing identity, so a start that does not reuse the room ends at
//! `no_identity` and creates nothing; `existing` therefore means the room was
//! reused. Plain HTTP on loopback, used by these test clients only.

use matrix_sdk::{
    Client, EncryptionState, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::{RequestConfig, SyncSettings},
    ruma::{room_id, user_id},
};
use persephone_contracts::ConversationStart;
use persephone_core::conversations;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, path_regex},
};

const ROOM: &str = "!dm:synthetic.test";

fn state_event(kind: &str, key: &str, sender: &str, content: Value) -> Value {
    json!({
        "type": kind, "state_key": key, "sender": sender, "content": content,
        "event_id": format!("${kind}-{key}"), "origin_server_ts": 1
    })
}

/// Join events for `members`.
fn joined(members: &[&str]) -> Vec<Value> {
    members
        .iter()
        .map(|user| state_event("m.room.member", user, user, json!({ "membership": "join" })))
        .collect()
}

/// A sync with the direct-message room `ROOM`: `members` joined (two, as the
/// summary counts), marked direct for Bob, with `m.room.encryption` in its
/// state when `encrypted`.
fn sync_reply(encrypted: bool, members: &[&str]) -> Value {
    let mut state = vec![state_event(
        "m.room.create",
        "",
        "@alice:synthetic.test",
        json!({ "room_version": "11" }),
    )];
    state.extend(joined(members));
    if encrypted {
        state.push(state_event(
            "m.room.encryption",
            "",
            "@alice:synthetic.test",
            json!({ "algorithm": "m.megolm.v1.aes-sha2" }),
        ));
    }
    json!({
        "next_batch": "s1",
        "account_data": { "events": [{
            "type": "m.direct",
            "content": { "@bob:synthetic.test": [ROOM] }
        }]},
        "rooms": { "join": { ROOM: {
            "summary": { "m.joined_member_count": 2, "m.invited_member_count": 0 },
            "state": { "events": state },
            "timeline": { "events": [], "limited": false }
        }}}
    })
}

/// A signed-in client after one sync with the direct-message room.
async fn synced(server: &MockServer, encrypted: bool, members: &[&str]) -> Client {
    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "versions": ["v1.11"] })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sync_reply(encrypted, members)))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/keys/upload"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "one_time_key_counts": { "signed_curve25519": 50 } })),
        )
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/keys/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "device_keys": {} })))
        .mount(server)
        .await;
    // As in Persephone's own client (`account.rs`): no retries, so a failed
    // request is answered at once.
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
    client.sync_once(SyncSettings::default()).await.unwrap();
    client
}

/// Answers a member-list request with the same members as sync.
async fn member_list(server: &MockServer, members: &[&str]) {
    let chunk = joined(members);
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/members$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "chunk": chunk })))
        .mount(server)
        .await;
}

const ALICE_AND_BOB: [&str; 2] = ["@alice:synthetic.test", "@bob:synthetic.test"];

/// Answers the room's encryption state lookup with `reply`, and expects it
/// `times` times.
async fn lookup(server: &MockServer, reply: ResponseTemplate, times: u64) {
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/_matrix/client/v3/rooms/[^/]+/state/m\.room\.encryption/?$",
        ))
        .respond_with(reply)
        .expect(times)
        .mount(server)
        .await;
}

/// Starts a conversation with Bob after one sync, with the room's member list
/// as `members`, and checks the room's encryption state as the SDK reported
/// it before the start.
fn start_with_bob(
    encrypted: bool,
    members: &[&str],
    reply: Option<ResponseTemplate>,
) -> (EncryptionState, ConversationStart) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let server = MockServer::start().await;
        member_list(&server, members).await;
        let client = synced(&server, encrypted, members).await;
        match reply {
            Some(reply) => lookup(&server, reply, 1).await,
            // The state is known: nothing may ask the homeserver.
            None => lookup(&server, ResponseTemplate::new(500), 0).await,
        }
        let state = client
            .get_room(room_id!("!dm:synthetic.test"))
            .unwrap()
            .encryption_state();
        let start = conversations::start(&client, user_id!("@bob:synthetic.test"))
            .await
            .unwrap();
        server.verify().await;
        (state, start)
    })
}

fn existing() -> ConversationStart {
    ConversationStart::Existing {
        room_id: ROOM.into(),
    }
}

/// GIVEN an encrypted direct-message room with Bob WHEN a conversation with
/// Bob starts THEN that room is reused, without asking the homeserver.
#[test]
fn given_an_encrypted_direct_room_then_it_is_reused() {
    let (state, start) = start_with_bob(true, &ALICE_AND_BOB, None);
    assert!(matches!(state, EncryptionState::Encrypted));
    assert_eq!(start, existing());
}

/// GIVEN a direct-message room with Bob whose state the homeserver says has
/// no encryption WHEN a conversation with Bob starts THEN the room is not
/// reused: the start goes on to Bob's identity, which he does not have.
#[test]
fn given_an_unencrypted_direct_room_then_it_is_not_reused() {
    let not_found = ResponseTemplate::new(404)
        .set_body_json(json!({ "errcode": "M_NOT_FOUND", "error": "no encryption" }));
    let (state, start) = start_with_bob(false, &ALICE_AND_BOB, Some(not_found));
    assert!(matches!(state, EncryptionState::Unknown));
    assert_eq!(start, ConversationStart::NoIdentity {});
}

/// GIVEN a direct-message room with Bob whose encryption sync did not show
/// WHEN the homeserver says it is encrypted THEN the room is reused.
#[test]
fn given_an_unknown_state_the_homeserver_calls_encrypted_then_the_room_is_reused() {
    let encrypted =
        ResponseTemplate::new(200).set_body_json(json!({ "algorithm": "m.megolm.v1.aes-sha2" }));
    let (state, start) = start_with_bob(false, &ALICE_AND_BOB, Some(encrypted));
    assert!(matches!(state, EncryptionState::Unknown));
    assert_eq!(start, existing());
}

/// GIVEN a direct-message room with Bob whose encryption sync did not show
/// WHEN the homeserver fails to tell THEN the room counts as unencrypted and
/// is not reused (fail closed).
#[test]
fn given_an_unknown_state_the_homeserver_cannot_tell_then_the_room_is_not_reused() {
    let (state, start) = start_with_bob(false, &ALICE_AND_BOB, Some(ResponseTemplate::new(500)));
    assert!(matches!(state, EncryptionState::Unknown));
    assert_eq!(start, ConversationStart::NoIdentity {});
}

/// GIVEN an encrypted room still marked direct for Bob, with two members
/// WHEN those members are Alice and Carol (Bob left, Carol joined, the direct
/// mark stayed) THEN the room is not reused.
#[test]
fn given_a_direct_room_someone_else_joined_then_it_is_not_reused() {
    let (state, start) = start_with_bob(
        true,
        &["@alice:synthetic.test", "@carol:synthetic.test"],
        None,
    );
    assert!(matches!(state, EncryptionState::Encrypted));
    assert_eq!(start, ConversationStart::NoIdentity {});
}
