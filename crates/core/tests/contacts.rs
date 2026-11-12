//! Which direct-message room a contact verification may use, with the real
//! SDK against a small mocked homeserver. Bob's direct mark covers `!a-old`
//! and `!dm`; the SDK picks `!a-old` first (its store keeps rooms in the
//! order sync listed them). In `!dm` Alice and Bob are both joined; who is
//! in `!a-old` varies by test. Carol's mark on another room must survive any
//! change. Sync answers with the `m.direct` the client last saved, so a
//! saved change comes back as it would from a homeserver. Plain HTTP on
//! loopback, used by these test clients only.

use matrix_sdk::{
    Client, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::{RequestConfig, SyncSettings},
    ruma::{OwnedUserId, user_id},
};
use persephone_contracts::SecurityError;
use persephone_core::contacts;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path, path_regex},
};

const OLD: &str = "!a-old:synthetic.test";
const DM: &str = "!dm:synthetic.test";
const CAROLS: &str = "!carol:synthetic.test";
const ALICE: &str = "@alice:synthetic.test";
const BOB: &str = "@bob:synthetic.test";
const CAROL: &str = "@carol:synthetic.test";

fn member(user: &str, membership: &str) -> Value {
    json!({
        "type": "m.room.member", "state_key": user, "sender": user,
        "content": { "membership": membership },
        "event_id": format!("${user}-{membership}"), "origin_server_ts": 1
    })
}

fn joined_room(members: &[Value]) -> Value {
    let mut state = vec![json!({
        "type": "m.room.create", "state_key": "", "sender": ALICE,
        "content": { "room_version": "11" },
        "event_id": "$create", "origin_server_ts": 1
    })];
    state.extend_from_slice(members);
    let count = |membership: &str| {
        members
            .iter()
            .filter(|event| event["content"]["membership"] == membership)
            .count()
    };
    json!({
        "summary": {
            "m.joined_member_count": count("join"),
            "m.invited_member_count": count("invite")
        },
        "state": { "events": state },
        "timeline": { "events": [], "limited": false }
    })
}

/// Answers sync with both rooms and the `m.direct` last saved. Each answer
/// has a new batch token: the SDK skips an answer with the token it has.
struct SyncReply {
    direct: Arc<Mutex<Value>>,
    old: Vec<Value>,
    dm: Vec<Value>,
    batch: AtomicU64,
}

impl Respond for SyncReply {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let direct = self.direct.lock().unwrap().clone();
        let batch = self.batch.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(json!({
            "next_batch": format!("s{batch}"),
            "account_data": { "events": [{ "type": "m.direct", "content": direct }] },
            "rooms": { "join": {
                OLD: joined_room(&self.old),
                DM: joined_room(&self.dm),
            }}
        }))
    }
}

/// Keeps a saved `m.direct`, so the next sync returns it.
struct SaveDirect(Arc<Mutex<Value>>);

impl Respond for SaveDirect {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        *self.0.lock().unwrap() = serde_json::from_slice(&request.body).unwrap();
        ResponseTemplate::new(200).set_body_json(json!({}))
    }
}

/// Answers a member-list request for the room whose ID holds `part`.
async fn member_list(server: &MockServer, part: &str, room: &str, reply: Option<&[Value]>) {
    let response = match reply {
        Some(members) => {
            let chunk: Vec<Value> = members
                .iter()
                .cloned()
                .map(|mut event| {
                    event["room_id"] = json!(room);
                    event
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "chunk": chunk }))
        }
        None => ResponseTemplate::new(500),
    };
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/_matrix/client/v3/rooms/[^/]*{part}[^/]*/members$"
        )))
        .respond_with(response)
        .mount(server)
        .await;
}

struct Setup {
    server: MockServer,
    client: Client,
    direct: Arc<Mutex<Value>>,
}

/// A signed-in client after one sync, with `!a-old`'s members as `old` (its
/// member list fails to load when `old_list` is false) and the `m.direct`
/// saves expected `saves` times.
async fn synced(old: &[Value], old_list: bool, saves: u64) -> Setup {
    let server = MockServer::start().await;
    let dm = vec![member(ALICE, "join"), member(BOB, "join")];
    member_list(&server, "old", OLD, old_list.then_some(old)).await;
    member_list(&server, "dm", DM, Some(&dm)).await;
    let direct = Arc::new(Mutex::new(json!({ BOB: [OLD, DM], CAROL: [CAROLS] })));
    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "versions": ["v1.19"] })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/sync"))
        .respond_with(SyncReply {
            direct: direct.clone(),
            old: old.to_vec(),
            dm,
            batch: AtomicU64::new(1),
        })
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/v3/user/[^/]+/account_data/m\.direct$",
        ))
        .respond_with(SaveDirect(direct.clone()))
        .expect(saves)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/keys/upload"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "one_time_key_counts": { "signed_curve25519": 50 } })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/keys/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "device_keys": {} })))
        .mount(&server)
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
    client.sync_once(SyncSettings::default()).await.unwrap();
    // Keeps syncing, as the session does, so a saved change comes back.
    let syncing = client.clone();
    tokio::spawn(async move {
        loop {
            let _ = syncing.sync_once(SyncSettings::default()).await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
    Setup {
        server,
        client,
        direct,
    }
}

fn bob() -> OwnedUserId {
    user_id!("@bob:synthetic.test").to_owned()
}

/// GIVEN an older room still marked direct for Bob, which Bob left and the
/// SDK picks first, and a room where Alice and Bob are both joined WHEN Alice
/// verifies Bob from the joined room THEN the older room loses Bob's mark
/// only, Carol's mark stays, and the SDK now picks the joined room.
#[tokio::test]
async fn given_a_room_bob_left_then_it_loses_his_mark_and_he_is_verifiable() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "leave")], true, 1).await;
    let picked = |client: &Client| {
        client
            .get_dm_room(&bob())
            .map(|room| room.room_id().to_string())
    };
    assert_eq!(picked(&setup.client).as_deref(), Some(OLD));

    assert_eq!(
        contacts::verifiable_contact(&setup.client, DM).await,
        Ok(bob())
    );

    assert_eq!(
        *setup.direct.lock().unwrap(),
        json!({ BOB: [DM], CAROL: [CAROLS] })
    );
    assert_eq!(picked(&setup.client).as_deref(), Some(DM));
    setup.server.verify().await;
}

/// GIVEN an older direct room where Bob's invitation is still pending WHEN
/// Alice verifies Bob as a member of the joined room THEN the older room
/// loses Bob's mark and he is verifiable.
#[tokio::test]
async fn given_a_room_bob_is_only_invited_to_then_a_member_is_verifiable() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "invite")], true, 1).await;

    assert_eq!(
        contacts::verifiable_member(&setup.client, DM, &bob()).await,
        Ok(())
    );

    assert_eq!(
        *setup.direct.lock().unwrap(),
        json!({ BOB: [DM], CAROL: [CAROLS] })
    );
    setup.server.verify().await;
}

/// GIVEN an older direct room whose member list cannot be loaded WHEN Alice
/// verifies Bob THEN nothing is changed and the start fails (fail closed: a
/// room is never unmarked on a guess).
#[tokio::test]
async fn given_an_unreadable_older_room_then_nothing_changes_and_it_fails() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "join")], false, 0).await;

    assert_eq!(
        contacts::verifiable_contact(&setup.client, DM).await,
        Err(SecurityError::Failed)
    );

    assert_eq!(
        *setup.direct.lock().unwrap(),
        json!({ BOB: [OLD, DM], CAROL: [CAROLS] })
    );
    setup.server.verify().await;
}

/// GIVEN an older direct room where Alice and Bob are both joined WHEN Alice
/// verifies Bob from the other joined room THEN the SDK's pick is already
/// fine and no mark changes.
#[tokio::test]
async fn given_two_good_rooms_then_no_mark_changes() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "join")], true, 0).await;

    assert_eq!(
        contacts::verifiable_contact(&setup.client, DM).await,
        Ok(bob())
    );

    setup.server.verify().await;
}

/// GIVEN a room marked direct for Bob that Carol joined after Bob left WHEN
/// Alice verifies Bob from it THEN it is not a direct-message room and no
/// mark changes.
#[tokio::test]
async fn given_a_room_someone_else_joined_then_it_is_not_direct() {
    let setup = synced(&[member(ALICE, "join"), member(CAROL, "join")], true, 0).await;

    assert_eq!(
        contacts::verifiable_contact(&setup.client, OLD).await,
        Err(SecurityError::NotDirectRoom)
    );

    setup.server.verify().await;
}
