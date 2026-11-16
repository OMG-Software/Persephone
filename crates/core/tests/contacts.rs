//! Which direct-message room a contact verification may use, with the real
//! SDK against a small mocked homeserver. Bob's direct mark covers `!a-old`
//! and `!dm`; the SDK picks `!a-old` first (its store keeps rooms in the
//! order sync listed them). In `!dm` Alice and Bob are both joined; who is
//! in `!a-old` varies by test. Carol's mark on another room must survive any
//! change. Sync answers with the `m.direct` the client last saved, so a
//! saved change comes back as it would from a homeserver. Plain HTTP on
//! loopback, used by these test clients only. The last tests check the
//! receiving side: which incoming requests are shown.

use matrix_sdk::{
    Client, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::{RequestConfig, SyncSettings},
    ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId, user_id},
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
const DAVES: &str = "!dave:synthetic.test";
const DAVE: &str = "@dave:synthetic.test";

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

/// Keeps a saved `m.direct`, so the next sync and read return it.
struct SaveDirect(Arc<Mutex<Value>>);

impl Respond for SaveDirect {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        *self.0.lock().unwrap() = serde_json::from_slice(&request.body).unwrap();
        ResponseTemplate::new(200).set_body_json(json!({}))
    }
}

/// Answers a read of `m.direct` with the homeserver's copy: the saved one
/// plus `newer`, marks another device added that this client has not synced.
struct ServeDirect {
    direct: Arc<Mutex<Value>>,
    newer: Arc<Mutex<Value>>,
}

impl Respond for ServeDirect {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut copy = self.direct.lock().unwrap().clone();
        for (user, rooms) in self.newer.lock().unwrap().as_object().unwrap() {
            copy[user] = rooms.clone();
        }
        ResponseTemplate::new(200).set_body_json(copy)
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
    newer: Arc<Mutex<Value>>,
}

/// A signed-in client after one sync, with `!a-old`'s members as `old` (its
/// member list fails to load when `old_list` is false) and the `m.direct`
/// saves expected `saves` times.
async fn synced(old: &[Value], old_list: bool, saves: u64) -> Setup {
    let server = MockServer::builder().start().await;
    let dm = vec![member(ALICE, "join"), member(BOB, "join")];
    member_list(&server, "old", OLD, old_list.then_some(old)).await;
    member_list(&server, "dm", DM, Some(&dm)).await;
    let direct = Arc::new(Mutex::new(json!({ BOB: [OLD, DM], CAROL: [CAROLS] })));
    let newer = Arc::new(Mutex::new(json!({})));
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
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/_matrix/client/v3/user/[^/]+/account_data/m\.direct$",
        ))
        .respond_with(ServeDirect {
            direct: direct.clone(),
            newer: newer.clone(),
        })
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
        newer,
    }
}

fn bob() -> OwnedUserId {
    user_id!("@bob:synthetic.test").to_owned()
}

/// GIVEN an older room still marked direct for Bob, which Bob left and the
/// SDK picks first, a room where Alice and Bob are both joined, and a mark
/// for Dave another device added that this client has not synced WHEN Alice
/// verifies Bob from the joined room THEN the older room loses Bob's mark
/// only, Carol's and Dave's marks stay, and the SDK now picks the joined
/// room.
#[tokio::test]
async fn given_a_room_bob_left_then_it_loses_his_mark_and_he_is_verifiable() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "leave")], true, 1).await;
    *setup.newer.lock().unwrap() = json!({ DAVE: [DAVES] });
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
        json!({ BOB: [DM], CAROL: [CAROLS], DAVE: [DAVES] })
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

fn room(id: &str) -> OwnedRoomId {
    RoomId::parse(id).unwrap()
}

fn user(id: &str) -> OwnedUserId {
    UserId::parse(id).unwrap()
}

/// Waits until sync has removed every direct mark this client holds for Bob.
async fn wait_unmarked(client: &Client) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while client.get_dm_rooms(&bob()).next().is_some() {
        assert!(tokio::time::Instant::now() < deadline, "marks not synced");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// Incoming requests. Both ways to start a verification (from a contact's
// room, or from a member of another room) send the request into the room the
// SDK picks for the contact, so the receiving side sees the same request
// either way: an in-room request from the other member. These tests are
// Alice's side, receiving a request from Bob.

/// GIVEN a room marked direct for Bob where Alice and Bob are both joined
/// WHEN Bob asks Alice to verify in it THEN the request is shown.
#[tokio::test]
async fn given_a_marked_room_with_just_us_then_bobs_request_is_shown() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "join")], true, 0).await;

    assert!(contacts::may_request(&setup.client, &room(DM), &bob()).await);

    setup.server.verify().await;
}

/// GIVEN a room where Alice and Bob are both joined that Alice's account does
/// not mark direct (Bob's client marked it, Alice's did not) WHEN Bob asks
/// Alice to verify in it THEN the request is still shown, and no mark is
/// written.
#[tokio::test]
async fn given_an_unmarked_room_with_just_us_then_bobs_request_is_shown() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "join")], true, 0).await;
    *setup.direct.lock().unwrap() = json!({ CAROL: [CAROLS] });
    wait_unmarked(&setup.client).await;

    assert!(contacts::may_request(&setup.client, &room(DM), &bob()).await);

    assert_eq!(*setup.direct.lock().unwrap(), json!({ CAROL: [CAROLS] }));
    setup.server.verify().await;
}

/// GIVEN a room marked direct for Bob that Carol also joined WHEN Bob asks
/// Alice to verify in it THEN the request is not shown: the mark does not
/// make up for a third member.
#[tokio::test]
async fn given_a_marked_room_with_a_third_member_then_bobs_request_is_not_shown() {
    let setup = synced(
        &[
            member(ALICE, "join"),
            member(BOB, "join"),
            member(CAROL, "join"),
        ],
        true,
        0,
    )
    .await;

    assert!(!contacts::may_request(&setup.client, &room(OLD), &bob()).await);

    setup.server.verify().await;
}

/// GIVEN a room where Bob is only invited WHEN a request from Bob is seen in
/// it THEN it is not shown: both must be joined.
#[tokio::test]
async fn given_bob_only_invited_then_his_request_is_not_shown() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "invite")], true, 0).await;

    assert!(!contacts::may_request(&setup.client, &room(OLD), &bob()).await);

    setup.server.verify().await;
}

/// GIVEN a room with just Alice and Bob WHEN the request names Carol, who is
/// not a member, or Alice herself as the sender THEN it is not shown.
#[tokio::test]
async fn given_a_sender_who_is_not_the_other_member_then_the_request_is_not_shown() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "join")], true, 0).await;

    assert!(!contacts::may_request(&setup.client, &room(DM), &user(CAROL)).await);
    assert!(!contacts::may_request(&setup.client, &room(DM), &user(ALICE)).await);

    setup.server.verify().await;
}

/// GIVEN a room whose member list cannot be loaded WHEN Bob asks Alice to
/// verify in it THEN the request is not shown (fail closed).
#[tokio::test]
async fn given_an_unreadable_member_list_then_bobs_request_is_not_shown() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "join")], false, 0).await;

    assert!(!contacts::may_request(&setup.client, &room(OLD), &bob()).await);

    setup.server.verify().await;
}

/// GIVEN a room this client does not know WHEN a request from Bob names it
/// THEN it is not shown.
#[tokio::test]
async fn given_an_unknown_room_then_bobs_request_is_not_shown() {
    let setup = synced(&[member(ALICE, "join"), member(BOB, "join")], true, 0).await;

    assert!(!contacts::may_request(&setup.client, &room("!unknown:synthetic.test"), &bob()).await);

    setup.server.verify().await;
}
