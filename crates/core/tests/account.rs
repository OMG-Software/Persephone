//! Sign-in, restore, sync and Matrix views against a mock homeserver. The mock
//! speaks plain HTTP on loopback, which only the `dev-disposable-servers`
//! feature accepts; `tools/dev.py check` runs this file with it.
#![cfg(feature = "dev-disposable-servers")]

use persephone_contracts::{
    AccountState, AccountStatus, ContactStatus, ContactTrust, DisplayName, HistoryVisibility,
    IdentityState, JoinRule, LevelKey, Membership, NoticeKind, PendingMessage, PendingState,
    PowerLevel, RoomChange, RoomEncryption, RoomIdentityChanges, RoomList, RoomMembers,
    RoomSettings, SecurityError, SecurityOperation, SecurityStatus, SendFailure, SignInError,
    StoreState, TimelineItem, TimelinePage,
};
use persephone_core::{
    Core, IDLE_CHECK, LOCK_TIMEOUT, ServeEnd,
    credentials::{CredentialSource, Credentials, PromptError, SecretPrompt},
    serve,
    session::AdmissionError,
};
use persephone_core_protocol::{Call, ErrorCode, Outcome, Request};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path, path_regex},
};
use zeroize::Zeroizing;

mod common;

const ROOM: &str = "!room:synthetic.test";
const USER: &str = "@alice:synthetic.test";
const TOKEN: &str = "syt_synthetic_access_token_7Q2";
const PASSWORD: &str = "synthetic-password-K8";
const ALIAS: &str = "#synthetic:synthetic.test";

/// Answers the prompts with fixed values, counts how often each was asked and
/// keeps the recovery key it is shown.
struct Fixed {
    answer: Result<(), PromptError>,
    asked: AtomicUsize,
    /// Answers in order; the last one repeats.
    secrets: std::sync::Mutex<Vec<Result<String, PromptError>>>,
    secrets_asked: AtomicUsize,
    shown: std::sync::Mutex<Option<String>>,
    /// What showing the recovery key returns.
    show: Result<(), PromptError>,
}

impl Fixed {
    fn new(answer: Result<(), PromptError>) -> Arc<Self> {
        Self::with_secret(answer, Ok(PASSWORD.into()))
    }

    fn with_secret(
        answer: Result<(), PromptError>,
        secret: Result<String, PromptError>,
    ) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: AtomicUsize::new(0),
            secrets: std::sync::Mutex::new(vec![secret]),
            secrets_asked: AtomicUsize::new(0),
            shown: std::sync::Mutex::new(None),
            show: Ok(()),
        })
    }

    /// A source whose recovery-key window fails at once.
    fn failing_window() -> Arc<Self> {
        Arc::new(Self {
            show: Err(PromptError::Failed),
            ..Arc::into_inner(Self::new(Ok(()))).unwrap()
        })
    }
}

impl CredentialSource for Fixed {
    fn ask(&self, _host: &str) -> Result<Credentials, PromptError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answer.map(|()| Credentials {
            username: Zeroizing::new("alice".into()),
            password: Zeroizing::new(PASSWORD.into()),
        })
    }

    fn ask_secret(&self, prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        self.secrets_asked.fetch_add(1, Ordering::SeqCst);
        if let SecretPrompt::Password { user_id, .. } = prompt {
            assert_eq!(user_id, USER, "the prompt names the signed-in account");
        }
        let mut secrets = self.secrets.lock().unwrap();
        let answer = if secrets.len() > 1 {
            secrets.remove(0)
        } else {
            secrets[0].clone()
        };
        answer.map(Zeroizing::new)
    }

    fn show_recovery_key(&self, user_id: &str, key: &str) -> Result<(), PromptError> {
        assert_eq!(user_id, USER);
        if self.show.is_ok() {
            *self.shown.lock().unwrap() = Some(key.to_owned());
        }
        self.show
    }
}

/// The full state of `ROOM`, a room version 11 room (Matrix v1.19): its
/// `m.room.create` has no `creator`, the sender is the creator. The join
/// rule, history setting and levels differ from the defaults a client
/// assumes without them, so a test can tell they arrived.
fn room_state() -> Value {
    json!([
        {"type":"m.room.create","state_key":"","sender":USER,"event_id":"$create","origin_server_ts":1,"content":{"room_version":"11"}},
        {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$join","origin_server_ts":2,"content":{"membership":"join"}},
        {"type":"m.room.power_levels","state_key":"","sender":USER,"event_id":"$levels","origin_server_ts":3,"content":{"users":{USER:100},"users_default":0,"events":{"m.room.name":50,"m.room.power_levels":100},"events_default":0,"state_default":50,"ban":50,"kick":50,"redact":50,"invite":0}},
        {"type":"m.room.join_rules","state_key":"","sender":USER,"event_id":"$rules","origin_server_ts":4,"content":{"join_rule":"public"}},
        {"type":"m.room.history_visibility","state_key":"","sender":USER,"event_id":"$history","origin_server_ts":5,"content":{"history_visibility":"joined"}},
        {"type":"m.room.name","state_key":"","sender":USER,"event_id":"$name","origin_server_ts":6,"content":{"name":"Synthetic <b>room</b>"}},
        {"type":"m.room.topic","state_key":"","sender":USER,"event_id":"$topic","origin_server_ts":7,"content":{"topic":"Synthetic topic"}},
        {"type":"m.room.encryption","state_key":"","sender":USER,"event_id":"$encryption","origin_server_ts":8,"content":{"algorithm":"m.megolm.v1.aes-sha2"}}
    ])
}

/// Whether a Sliding Sync `required_state` entry (`[type, state_key]`) asks
/// for `event`; `*` matches any, `$ME` is this account.
fn requested(entry: &Value, event: &Value) -> bool {
    let kind = entry[0].as_str().unwrap_or_default();
    let key = match entry[1].as_str().unwrap_or_default() {
        "$ME" => USER,
        key => key,
    };
    (kind == "*" || event["type"] == kind) && (key == "*" || event["state_key"] == key)
}

/// Native Sliding Sync (MSC4186, which Matrix v1.19 does not include) like a
/// server answers it: `ROOM` carries only the state that the request's list
/// asks for, as the SDK sends that list with every request, less the
/// `withheld` event types.
struct SlidingRoom {
    withheld: &'static [&'static str],
}

/// The state a server that first sent `ROOM` before the list asked for more
/// leaves out: the list then asked only for these five of `room_state`'s
/// types besides the room's name, address, encryption, creation and this
/// account's membership.
const LATER_STATE: &[&str] = &[
    "m.room.power_levels",
    "m.room.join_rules",
    "m.room.history_visibility",
    "m.room.topic",
    "m.room.guest_access",
];

impl wiremock::Respond for SlidingRoom {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let wanted = body["lists"]["rooms"]["required_state"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let state: Vec<Value> = room_state()
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| wanted.iter().any(|entry| requested(entry, event)))
            .filter(|event| {
                !self
                    .withheld
                    .iter()
                    .any(|kind| event["type"].as_str() == Some(kind))
            })
            .cloned()
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "pos":"sliding-pos","lists":{"rooms":{"count":1}},
            "rooms":{ROOM:{"initial":true,"joined_count":1,"invited_count":0,"required_state":state,"timeline":[text("$live","live")],"num_live":1,"prev_batch":"history","notification_count":0,"highlight_count":0}},
            "extensions":{"e2ee":{"device_lists":{"changed":[],"left":[]},"device_one_time_keys_count":{"signed_curve25519":50}},"to_device":{"events":[],"next_batch":"to-device-pos"}}
        }))
    }
}

fn text(id: &str, body: &str) -> Value {
    json!({"type":"m.room.message","sender":USER,"event_id":id,"origin_server_ts":10,"content":{"msgtype":"m.text","body":body}})
}

fn edit_of(id: &str, sender: &str, replaces: &str, body: &str) -> Value {
    json!({"type":"m.room.message","sender":sender,"event_id":id,"origin_server_ts":11,"content":{
        "msgtype":"m.text","body":format!("* {body}"),
        "m.new_content":{"msgtype":"m.text","body":body},
        "m.relates_to":{"rel_type":"m.replace","event_id":replaces}
    }})
}

/// `events` with the `room_id` that Matrix v1.19 requires of a ClientEvent,
/// as `/messages` and `/members` return them (sync leaves it out).
fn in_room(room_id: &str, mut events: Value) -> Value {
    for event in events.as_array_mut().unwrap() {
        event["room_id"] = json!(room_id);
    }
    events
}

/// History of `ROOM`, newest first as `/messages` backward returns it.
/// `$text` has an edit by its sender and a forged one by someone else.
fn history() -> Value {
    in_room(
        ROOM,
        json!([
            edit_of("$forged", "@mallory:synthetic.test", "$text", "I agree to everything"),
            edit_of("$edit", USER, "$text", "Hello, corrected"),
            text("$text", "Hello from <script>alert(1)</script>"),
            {"type":"m.room.message","sender":USER,"event_id":"$image","origin_server_ts":9,"content":{"msgtype":"m.image","body":"cat.png","url":"mxc://synthetic.test/cat"}},
            {"type":"m.room.encrypted","sender":USER,"event_id":"$secret","origin_server_ts":8,"content":{"algorithm":"m.megolm.v1.aes-sha2","ciphertext":"AwgAEnACgAkLmt6qF84IK","device_id":"OTHER","sender_key":"c2VuZGVyLWtleQ","session_id":"c2Vzc2lvbi1pZA"}},
            {"type":"m.reaction","sender":USER,"event_id":"$reaction","origin_server_ts":7,"content":{"m.relates_to":{"rel_type":"m.annotation","event_id":"$text","key":"+1"}}}
        ]),
    )
}

/// `GET /rooms/{roomId}/state/{eventType}/` as Matrix v1.19 answers it for
/// `ROOM`: the content of its event of that type, or 404 `M_NOT_FOUND`.
struct RoomStateEvent;

impl wiremock::Respond for RoomStateEvent {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let kind = request
            .url
            .path_segments()
            .and_then(|mut segments| segments.nth(6))
            .unwrap_or_default()
            .to_owned();
        match room_state()
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["type"] == kind.as_str() && event["state_key"] == "")
        {
            Some(event) => ResponseTemplate::new(200).set_body_json(event["content"].clone()),
            None => ResponseTemplate::new(404)
                .set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "no such state"})),
        }
    }
}

/// A `/members` answer with these member events in the requested room.
struct Members(Value);

impl wiremock::Respond for Members {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        // `/_matrix/client/v3/rooms/{roomId}/members`
        let room_id = request
            .url
            .path_segments()
            .and_then(|mut segments| segments.nth(4))
            .unwrap_or_default()
            .replace("%21", "!")
            .replace("%3A", ":");
        ResponseTemplate::new(200)
            .set_body_json(json!({"chunk": in_room(&room_id, self.0.clone())}))
    }
}

/// Classic sync like a live room: each response has a new batch token and a
/// new message.
#[derive(Default)]
struct LiveRoom {
    round: AtomicUsize,
    /// Also report `PLAIN`, a room that is not encrypted.
    plain: bool,
    /// Also report `DM`, a direct-message room with `CONTACT`.
    dm: bool,
}

/// A joined, encrypted direct-message room with `CONTACT`; it sorts before
/// `ROOM`.
const DM: &str = "!dm:synthetic.test";
const CONTACT: &str = "@bob:synthetic.test";
/// Still marked as direct with `CONTACT`, but a third person joined it.
const GROUP: &str = "!group:synthetic.test";

fn group_state() -> Value {
    let mut state = dm_state();
    state.as_array_mut().unwrap().push(json!(
        {"type":"m.room.member","state_key":"@carol:synthetic.test","sender":"@carol:synthetic.test","event_id":"$gcarol","origin_server_ts":5,"content":{"membership":"join"}}
    ));
    state
}

fn dm_state() -> Value {
    json!([
        {"type":"m.room.create","state_key":"","sender":USER,"event_id":"$dcreate","origin_server_ts":1,"content":{"room_version":"11"}},
        {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$djoin","origin_server_ts":2,"content":{"membership":"join"}},
        {"type":"m.room.member","state_key":CONTACT,"sender":CONTACT,"event_id":"$dbob","origin_server_ts":3,"content":{"membership":"join"}},
        {"type":"m.room.encryption","state_key":"","sender":USER,"event_id":"$dencryption","origin_server_ts":4,"content":{"algorithm":"m.megolm.v1.aes-sha2"}}
    ])
}

/// A joined room without encryption; it sorts after `ROOM`.
const PLAIN: &str = "!zplain:synthetic.test";

fn plain_state() -> Value {
    json!([
        {"type":"m.room.create","state_key":"","sender":USER,"event_id":"$pcreate","origin_server_ts":1,"content":{"room_version":"11"}},
        {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$pjoin","origin_server_ts":2,"content":{"membership":"join"}}
    ])
}

impl wiremock::Respond for LiveRoom {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        let n = self.round.fetch_add(1, Ordering::SeqCst);
        let mut rooms = json!({ROOM:{"state":{"events":room_state()},"timeline":{"events":[text(&format!("$live{n}"), "live")],"limited":true,"prev_batch":"history"},"ephemeral":{"events":[]},"account_data":{"events":[]}}});
        if self.plain {
            rooms[PLAIN] = json!({"state":{"events":plain_state()},"timeline":{"events":[],"limited":false},"ephemeral":{"events":[]},"account_data":{"events":[]}});
        }
        let mut account_data = json!([]);
        if self.dm {
            rooms[DM] = json!({"summary":{"m.joined_member_count":2,"m.invited_member_count":0},"state":{"events":dm_state()},"timeline":{"events":[],"limited":false},"ephemeral":{"events":[]},"account_data":{"events":[]}});
            rooms[GROUP] = json!({"summary":{"m.joined_member_count":3,"m.invited_member_count":0},"state":{"events":group_state()},"timeline":{"events":[],"limited":false},"ephemeral":{"events":[]},"account_data":{"events":[]}});
            account_data = json!([{"type":"m.direct","content":{CONTACT:[DM, GROUP]}}]);
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "next_batch": format!("classic-{n}"),
            "account_data":{"events":account_data},
            "rooms":{"join":rooms},
            "to_device":{"events":[]},"device_lists":{"changed":[],"left":[]},"device_one_time_keys_count":{"signed_curve25519":50}
        }))
    }
}

struct Homeserver {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
}

impl Homeserver {
    /// `login`: the status and body the login endpoint answers with.
    /// `sliding`: advertise native Sliding Sync. `versions_ok`: whether
    /// capability discovery succeeds.
    fn start(login: (u16, Value), sliding: bool, versions_ok: bool) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(async {
            let server = MockServer::start().await;
            // A Matrix v1.19 homeserver; Sliding Sync is an unstable feature.
            let versions = if sliding {
                json!({"versions":["v1.19"],"unstable_features":{"org.matrix.simplified_msc3575":true}})
            } else {
                json!({"versions":["v1.19"]})
            };
            Mock::given(method("GET"))
                .and(path("/_matrix/client/versions"))
                .respond_with(if versions_ok {
                    ResponseTemplate::new(200).set_body_json(versions)
                } else {
                    ResponseTemplate::new(500).set_body_json(json!({"errcode":"M_UNKNOWN","error":"down"}))
                })
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/logout"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/login"))
                .respond_with(ResponseTemplate::new(login.0).set_body_json(login.1))
                .mount(&server)
                .await;
            for (p, body) in [
                ("/_matrix/client/v3/keys/upload", json!({"one_time_key_counts":{"signed_curve25519":50}})),
                ("/_matrix/client/v3/keys/query", json!({"device_keys":{},"failures":{}})),
            ] {
                Mock::given(method("POST"))
                    .and(path(p))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body))
                    .mount(&server)
                    .await;
            }
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/sync"))
                .respond_with(LiveRoom {
                    round: AtomicUsize::new(0),
                    plain: false,
                    dm: false,
                })
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"))
                .respond_with(SlidingRoom { withheld: &[] })
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/messages$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({"start":"history","end":"older","chunk":history(),"state":[]}),
                ))
                .mount(&server)
                .await;
            server
        });
        Self { runtime, server }
    }

    /// Answers the cross-signing upload with an authentication round until it
    /// carries `PASSWORD` and the round's session, and accepts the signature,
    /// account data and key backup requests that setting up recovery makes.
    fn encryption_endpoints(&self) {
        self.runtime.block_on(async {
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/device_signing/upload"))
                .and(body_partial_json(json!({"auth": {
                    "type": "m.login.password",
                    "password": PASSWORD,
                    "session": "uia-session-1",
                    "identifier": {"type": "m.id.user", "user": USER}
                }})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .with_priority(1)
                .mount(&self.server)
                .await;
            // Another password fails the step: the same answer with the
            // error (Matrix v1.19, user-interactive authentication).
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/device_signing/upload"))
                .and(body_partial_json(json!({"auth": {}})))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                    "errcode": "M_FORBIDDEN",
                    "error": "Invalid password",
                    "flows": [{"stages": ["m.login.password"]}],
                    "params": {},
                    "session": "uia-session-1"
                })))
                .with_priority(2)
                .mount(&self.server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/device_signing/upload"))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                    "flows": [{"stages": ["m.login.password"]}],
                    "params": {},
                    "session": "uia-session-1"
                })))
                .with_priority(3)
                .mount(&self.server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/signatures/upload"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"failures": {}})))
                .mount(&self.server)
                .await;
            Mock::given(method("PUT"))
                .and(path_regex(
                    r"^/_matrix/client/v3/user/[^/]+/account_data/[^/]+$",
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(
                    r"^/_matrix/client/v3/user/[^/]+/account_data/[^/]+$",
                ))
                .respond_with(
                    ResponseTemplate::new(404)
                        .set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "none"})),
                )
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/room_keys/version"))
                .respond_with(
                    ResponseTemplate::new(404)
                        .set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "no backup"})),
                )
                .mount(&self.server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/room_keys/version"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"version": "1"})))
                .mount(&self.server)
                .await;
            Mock::given(method("PUT"))
                .and(path("/_matrix/client/v3/room_keys/keys"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"etag": "1", "count": 0})),
                )
                .mount(&self.server)
                .await;
        });
    }

    /// Adds the unencrypted room `PLAIN` to every sync, the member lists the
    /// SDK loads before sharing keys, and a send endpoint that answers
    /// `status` with an event ID.
    fn sending_endpoints(&self, status: u16) {
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/sync"))
                .respond_with(LiveRoom {
                    round: AtomicUsize::new(0),
                    plain: true,
                    dm: false,
                })
                .with_priority(1)
                .mount(&self.server)
                .await;
            // The SDK asks for a room's encryption state when sync left it
            // unknown (a send can come before the first sync): the plain room
            // has none, the others the state that sync gives them.
            Mock::given(method("GET"))
                .and(path_regex(
                    r"^/_matrix/client/v3/rooms/[^/]*zplain[^/]*/state/m\.room\.encryption/?$",
                ))
                .respond_with(ResponseTemplate::new(404).set_body_json(
                    json!({"errcode": "M_NOT_FOUND", "error": "no encryption"}),
                ))
                .with_priority(1)
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/state/m\.room\.encryption/?$"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"algorithm": "m.megolm.v1.aes-sha2"})),
                )
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/members$"))
                .respond_with(Members(json!([
                    {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$m","origin_server_ts":2,"content":{"membership":"join"}}
                ])))
                .mount(&self.server)
                .await;
            Mock::given(method("PUT"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/send/[^/]+/[^/]+$"))
                .respond_with(if status == 200 {
                    ResponseTemplate::new(200).set_body_json(json!({"event_id": "$sent"}))
                } else {
                    ResponseTemplate::new(status)
                        .set_body_json(json!({"errcode": "M_UNKNOWN", "error": "refused"}))
                })
                .mount(&self.server)
                .await;
        });
    }

    /// Adds the direct-message room `DM` with `CONTACT` to every sync, with
    /// both as joined members.
    fn direct_endpoints(&self) {
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/sync"))
                .respond_with(LiveRoom {
                    round: AtomicUsize::new(0),
                    plain: false,
                    dm: true,
                })
                .with_priority(1)
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]*dm[^/]*/members$"))
                .respond_with(Members(json!([
                    {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$djoin","origin_server_ts":2,"content":{"membership":"join"}},
                    {"type":"m.room.member","state_key":CONTACT,"sender":CONTACT,"event_id":"$dbob","origin_server_ts":3,"content":{"membership":"join"}}
                ])))
                .with_priority(1)
                .mount(&self.server)
                .await;
        });
    }

    /// The requests the room settings make besides synced state, answered
    /// as Matrix v1.19 specifies: `ROOM` is not in the public directory, has
    /// the local alias `ALIAS`, and `CONTACT` is invited. Its state events
    /// are `room_state`; it has no guest access event.
    fn settings_endpoints(&self) {
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/state/[^/]+/?$"))
                .respond_with(RoomStateEvent)
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/directory/list/room/[^/]+$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"visibility": "private"})),
                )
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/aliases$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"aliases": [ALIAS]})))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/v3/rooms/[^/]+/members$"))
                .respond_with(Members(json!([
                    {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$join","origin_server_ts":2,"content":{"membership":"join"}},
                    {"type":"m.room.member","state_key":CONTACT,"sender":USER,"event_id":"$invite","origin_server_ts":9,"content":{"membership":"invite"}}
                ])))
                .mount(&self.server)
                .await;
        });
    }

    /// Answers the next `method` request for this account's display name
    /// with `status` and `body`.
    fn display_name_answers(&self, method_name: &str, status: u16, body: Value) {
        self.runtime.block_on(
            Mock::given(method(method_name))
                .and(path_regex(
                    r"^/_matrix/client/v3/profile/[^/]+/displayname$",
                ))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .up_to_n_times(1)
                .mount(&self.server),
        );
    }

    /// Requests of `method` to this account's display name, in order.
    fn display_name_requests(&self, method_name: &str) -> Vec<Value> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| {
                request.method.as_str() == method_name
                    && request
                        .url
                        .path()
                        .starts_with("/_matrix/client/v3/profile/")
                    && request.url.path().ends_with("/displayname")
            })
            .map(|request| serde_json::from_slice(&request.body).unwrap_or(Value::Null))
            .collect()
    }

    /// Answers sends with 200 from now on.
    fn accept_sends(&self) {
        self.runtime.block_on(
            Mock::given(method("PUT"))
                .and(path_regex(
                    r"^/_matrix/client/v3/rooms/[^/]+/send/[^/]+/[^/]+$",
                ))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"event_id": "$sent"})),
                )
                .with_priority(1)
                .mount(&self.server),
        );
    }

    /// Paths of the send requests, in order.
    fn sends(&self) -> Vec<String> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| {
                request.method.as_str() == "PUT" && request.url.path().contains("/send/")
            })
            .map(|request| request.url.path().to_owned())
            .collect()
    }

    /// Bodies of the requests made to `endpoint`, in order.
    fn bodies_to(&self, endpoint: &str) -> Vec<Value> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == endpoint)
            .map(|request| serde_json::from_slice(&request.body).unwrap_or(Value::Null))
            .collect()
    }

    fn signed_in() -> Self {
        Self::start(
            (
                200,
                json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}),
            ),
            false,
            true,
        )
    }

    /// Makes logout fail from now on (the server cannot revoke).
    fn fail_logout(&self) {
        self.runtime.block_on(
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/logout"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_json(json!({"errcode":"M_UNKNOWN","error":"down"})),
                )
                .with_priority(1)
                .mount(&self.server),
        );
    }

    /// Makes logout answer only after `delay`, so a test can look at the
    /// profile while the request is in flight.
    fn slow_logout(&self, delay: Duration) {
        self.runtime.block_on(
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/logout"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({}))
                        .set_delay(delay),
                )
                .with_priority(1)
                .mount(&self.server),
        );
    }

    /// Refuses the access token in sync from now on, as a homeserver does
    /// once the device was signed out elsewhere or the account was reset or
    /// removed; `soft_logout` as the spec's soft logout. `whoami_too`:
    /// whether `whoami` refuses it as well, or still answers.
    fn refuse_token(&self, soft_logout: bool, whoami_too: bool) {
        let refused = ResponseTemplate::new(401).set_body_json(json!({
            "errcode": "M_UNKNOWN_TOKEN",
            "error": "Unknown access token",
            "soft_logout": soft_logout
        }));
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/sync"))
                .respond_with(refused.clone())
                .with_priority(1)
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/account/whoami"))
                .respond_with(if whoami_too {
                    refused
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"user_id": USER, "device_id": "PERSEPHONE1"}))
                })
                .with_priority(1)
                .mount(&self.server)
                .await;
        });
    }

    /// Refuses every refresh token from now on, as a homeserver does once it
    /// ended the session.
    fn refuse_refresh(&self) {
        self.runtime.block_on(
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/refresh"))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                    "errcode": "M_UNKNOWN_TOKEN",
                    "error": "Unknown refresh token",
                    "soft_logout": false
                })))
                .mount(&self.server),
        );
    }

    /// Refuses the access token without soft logout in one sync round and
    /// its `whoami`, then answers as before.
    fn refuse_token_once(&self) {
        let refused = ResponseTemplate::new(401).set_body_json(json!({
            "errcode": "M_UNKNOWN_TOKEN",
            "error": "Unknown access token",
            "soft_logout": false
        }));
        self.runtime.block_on(async {
            for (verb, endpoint) in [
                ("GET", "/_matrix/client/v3/sync"),
                ("GET", "/_matrix/client/v3/account/whoami"),
            ] {
                Mock::given(method(verb))
                    .and(path(endpoint))
                    .respond_with(refused.clone())
                    .up_to_n_times(1)
                    .with_priority(1)
                    .mount(&self.server)
                    .await;
            }
        });
    }

    fn uri(&self) -> String {
        self.server.uri()
    }

    fn requests_to(&self, endpoint: &str) -> usize {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == endpoint)
            .count()
    }

    /// Requests for a state event of type `kind` of any room.
    fn state_requests(&self, kind: &str) -> usize {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| {
                request.method.as_str() == "GET"
                    && request.url.path_segments().is_some_and(|segments| {
                        let segments: Vec<_> = segments.collect();
                        segments.get(5) == Some(&"state") && segments.get(6) == Some(&kind)
                    })
            })
            .count()
    }

    /// Sliding Sync like a server that sent `ROOM` on this connection before
    /// the list asked for `LATER_STATE`, and does not send it now: per room
    /// it sends only state that changed since.
    fn withhold_later_state(&self) {
        self.runtime.block_on(
            Mock::given(method("POST"))
                .and(path(
                    "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync",
                ))
                .respond_with(SlidingRoom {
                    withheld: LATER_STATE,
                })
                .with_priority(1)
                .mount(&self.server),
        );
    }

    /// Accepts every state event sent to a room.
    fn state_events_accepted(&self) {
        self.runtime.block_on(
            Mock::given(method("PUT"))
                .and(path_regex(
                    r"^/_matrix/client/v3/rooms/[^/]+/state/[^/]+/?$",
                ))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"event_id": "$sent"})),
                )
                .mount(&self.server),
        );
    }

    /// The content of the last power levels sent to a room.
    fn sent_power_levels(&self) -> Value {
        let requests = self
            .runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default();
        let sent = requests
            .iter()
            .rev()
            .find(|request| {
                request.method.as_str() == "PUT"
                    && request.url.path().contains("/state/m.room.power_levels")
            })
            .expect("power levels were sent");
        serde_json::from_slice(&sent.body).unwrap()
    }

    /// Answers at most the next two requests for `ROOM`'s `kind` state
    /// event (a settings and a members read) with `status` and `body`, ahead
    /// of `settings_endpoints`.
    fn state_answers(&self, kind: &str, status: u16, body: Value) {
        self.runtime.block_on(
            Mock::given(method("GET"))
                .and(path_regex(format!(
                    r"^/_matrix/client/v3/rooms/[^/]+/state/{}/?$",
                    kind.replace('.', r"\.")
                )))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .with_priority(1)
                .up_to_n_times(2)
                .mount(&self.server),
        );
    }
}

fn make_core(dir: &Path, source: Arc<dyn CredentialSource>) -> Core {
    Core::default()
        .with_credentials(source)
        .with_session(common::open_session(dir))
}

fn call(core: &Core, call: Call) -> Outcome {
    core.handle(&Request { id: 1, call }).outcome
}

fn value<T: serde::de::DeserializeOwned>(outcome: Outcome) -> T {
    match outcome {
        Outcome::Ok(value) => serde_json::from_value(value).unwrap(),
        other => panic!("{other:?}"),
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sign_in(core: &Core, homeserver: &str) -> AccountStatus {
    value(call(
        core,
        Call::SignIn {
            homeserver: homeserver.into(),
        },
    ))
}

/// A core signed in to `server`, once `ROOM` is listed with its name.
fn core_with_room(server: &Homeserver, dir: &Path) -> Core {
    let core = make_core(dir, Fixed::new(Ok(())));
    sign_in_to_room(&core, server);
    core
}

/// Signs `core` in to `server` and waits until `ROOM` is listed with its
/// name. A test that guards the core with `LockOnDrop` first stops the sync
/// loop even when this times out.
fn sign_in_to_room(core: &Core, server: &Homeserver) {
    sign_in(core, &server.uri());
    // The SDK lists a room before it has computed its name; wait for both.
    wait_for("the room", || {
        value::<RoomList>(call(core, Call::RoomList {}))
            .rooms
            .first()
            .is_some_and(|room| room.display_name.is_some())
    });
}

fn files_contain(root: &Path, needle: &[u8]) -> Vec<String> {
    let mut hits = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
            {
                hits.push(path.display().to_string());
            }
        }
    }
    hits
}

#[test]
fn given_valid_credentials_then_rooms_and_history_come_from_the_homeserver_and_survive_a_restart() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let source = Fixed::new(Ok(()));
    {
        let core = make_core(dir.path(), source.clone());
        let status = sign_in(&core, &server.uri());
        assert_eq!(status.state, AccountState::SigningIn);
        wait_for("sign-in", || {
            core.session().account_status().state == AccountState::SignedIn
        });
        assert_eq!(
            core.session().account_status().user_id.as_deref(),
            Some(USER)
        );
        assert_eq!(source.asked.load(Ordering::SeqCst), 1);
        assert_eq!(server.requests_to("/_matrix/client/v3/login"), 1);

        // The SDK lists a room before it has computed its name; wait for both.
        wait_for("the first sync", || {
            value::<RoomList>(call(&core, Call::RoomList {}))
                .rooms
                .first()
                .is_some_and(|room| room.display_name.is_some())
        });
        // A homeserver that answers at once (this mock does not long-poll)
        // must not make the core spin: rounds start at most every 500 ms.
        let before = server.requests_to("/_matrix/client/v3/sync");
        std::thread::sleep(Duration::from_secs(2));
        let rounds = server.requests_to("/_matrix/client/v3/sync") - before;
        assert!(rounds <= 5, "{rounds} sync rounds in 2 s");
        let rooms: RoomList = value(call(&core, Call::RoomList {}));
        assert_eq!(rooms.rooms[0].room_id, ROOM);
        assert_eq!(
            rooms.rooms[0].display_name.as_deref(),
            Some("Synthetic <b>room</b>")
        );

        let page: TimelinePage = value(call(
            &core,
            Call::TimelinePage {
                room_id: ROOM.into(),
                cursor: None,
            },
        ));
        // Oldest first; nothing is hidden silently. The reaction names the
        // message it reacts to. Only the sender's own edit is an edit: the
        // forged one, by someone else, is left out.
        let shown: Vec<String> = page
            .items
            .iter()
            .map(|item| match item {
                TimelineItem::Message { event_id, body, .. } => format!("{event_id}={body}"),
                TimelineItem::Notice {
                    event_id, notice, ..
                } => format!("{event_id}:{notice:?}"),
                TimelineItem::Image {
                    event_id,
                    name,
                    encrypted,
                    ..
                } => format!("{event_id}:image:{name}:{encrypted}"),
                TimelineItem::Gap { .. } => "gap".into(),
                TimelineItem::Edit {
                    event_id,
                    replaces,
                    body,
                    ..
                } => format!("{event_id}:edit:{replaces}={body}"),
                TimelineItem::State { event_id, .. } => format!("{event_id}:state"),
                TimelineItem::File { event_id, name, .. } => format!("{event_id}:file:{name}"),
                TimelineItem::Reaction {
                    event_id,
                    reacts_to,
                    key,
                    ..
                } => format!("{event_id}:reacts:{reacts_to}:{key}"),
                TimelineItem::Redaction {
                    event_id, redacts, ..
                } => format!("{event_id}:redacts:{redacts}"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "$reaction:reacts:$text:+1".to_string(),
                format!("$secret:{:?}", NoticeKind::Undecryptable),
                "$image:image:cat.png:false".to_string(),
                "$text=Hello from <script>alert(1)</script>".to_string(),
                "$edit:edit:$text=Hello, corrected".to_string(),
            ]
        );
        assert_eq!(page.next_cursor.as_deref(), Some("m1.0"));
        let older: TimelinePage = value(call(
            &core,
            Call::TimelinePage {
                room_id: ROOM.into(),
                cursor: Some("m1.0".into()),
            },
        ));
        assert_eq!(older.next_cursor.as_deref(), Some("m1.1"));
        assert_eq!(
            call(
                &core,
                Call::TimelinePage {
                    room_id: ROOM.into(),
                    cursor: Some("m1.99".into())
                }
            ),
            Outcome::Error(ErrorCode::NotFound)
        );

        // Sync rounds that touch a subscribed room become one coalesced event.
        assert_eq!(
            call(
                &core,
                Call::Subscribe {
                    room_id: ROOM.into()
                }
            ),
            Outcome::Ok(Value::Null)
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while core.pending_notifications() != 1 {
            assert!(
                Instant::now() < deadline,
                "no room event: status {:?}; sync requests {}",
                core.session().account_status(),
                server.requests_to("/_matrix/client/v3/sync")
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            call(
                &core,
                Call::Subscribe {
                    room_id: "!other:synthetic.test".into()
                }
            ),
            Outcome::Error(ErrorCode::NotFound)
        );

        // A Matrix client ran: locking closes everything and needs a restart.
        let state: StoreState = value(call(&core, Call::LockSession {}));
        assert_eq!(state, StoreState::RestartRequired);
        let state: StoreState = value(call(&core, Call::UnlockSession {}));
        assert_eq!(state, StoreState::RestartRequired);
        assert_eq!(
            call(&core, Call::RoomList {}),
            Outcome::Error(ErrorCode::Locked)
        );
    }

    // The session was saved encrypted: neither the token nor the password is in
    // any profile file.
    assert!(files_contain(dir.path(), TOKEN.as_bytes()).is_empty());
    assert!(files_contain(dir.path(), PASSWORD.as_bytes()).is_empty());

    // A restart restores the account without asking for a password.
    let never = Fixed::new(Err(PromptError::Failed));
    let core = make_core(dir.path(), never.clone());
    wait_for("restore", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    assert_eq!(never.asked.load(Ordering::SeqCst), 0);
    assert_eq!(
        server.requests_to("/_matrix/client/v3/login"),
        1,
        "no new login"
    );
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

#[test]
fn given_a_refused_password_or_a_cancelled_prompt_then_the_status_says_so() {
    let server = Homeserver::start(
        (
            403,
            json!({"errcode":"M_FORBIDDEN","error":"Invalid password"}),
        ),
        false,
        true,
    );
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    sign_in(&core, &server.uri());
    wait_for("refusal", || {
        core.session().account_status().state == AccountState::SignInFailed
    });
    assert_eq!(
        core.session().account_status().error,
        Some(SignInError::InvalidCredentials)
    );

    let cancelled = TempDir::new().unwrap();
    let source = Fixed::new(Err(PromptError::Cancelled));
    let core = make_core(cancelled.path(), source.clone());
    sign_in(&core, &server.uri());
    wait_for("cancel", || {
        core.session().account_status().error == Some(SignInError::Cancelled)
    });
    assert_eq!(
        server.requests_to("/_matrix/client/v3/login"),
        1,
        "cancel sends nothing"
    );
    // No client ran, so a plain lock (not a restart) follows.
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::Locked);
}

#[test]
fn given_bad_homeserver_urls_or_no_profile_then_sign_in_is_refused_before_any_prompt() {
    let dir = TempDir::new().unwrap();
    let source = Fixed::new(Ok(()));
    let core = make_core(dir.path(), source.clone());
    for bad in [
        "matrix.example.org",
        "http://192.168.1.10:8008",
        "http://localhost:8008",
        "https://user:pw@matrix.example.org",
    ] {
        assert_eq!(
            call(
                &core,
                Call::SignIn {
                    homeserver: bad.into()
                }
            ),
            Outcome::Error(ErrorCode::InvalidArgument),
            "{bad}"
        );
    }
    assert_eq!(
        call(
            &core,
            Call::SignIn {
                homeserver: "h".repeat(3000)
            }
        ),
        Outcome::Error(ErrorCode::InvalidArgument)
    );
    assert_eq!(source.asked.load(Ordering::SeqCst), 0);

    let without_profile = Core::default().with_credentials(source.clone());
    assert_eq!(
        call(
            &without_profile,
            Call::SignIn {
                homeserver: "https://matrix.example.org".into()
            }
        ),
        Outcome::Error(ErrorCode::Unavailable)
    );
    assert_eq!(source.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn given_native_sliding_sync_then_it_is_used_and_rooms_arrive() {
    let server = Homeserver::start(
        (
            200,
            json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}),
        ),
        true,
        true,
    );
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    sign_in(&core, &server.uri());
    wait_for("sliding rooms", || {
        !value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .is_empty()
    });
    assert!(server.requests_to("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync") > 0);
    assert_eq!(
        server.requests_to("/_matrix/client/v3/sync"),
        0,
        "no classic fallback"
    );
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN `ROOM`'s state from Sliding Sync (`sliding`) or classic sync WHEN
/// its settings and members are read THEN they show the room's own levels,
/// join rule, history setting and topic, not the defaults a client assumes
/// without them.
fn settings_come_from_the_room_state(sliding: bool) {
    let server = Homeserver::start(
        (
            200,
            json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}),
        ),
        sliding,
        true,
    );
    server.settings_endpoints();
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    let _locked = LockOnDrop(&core);
    sign_in_to_room(&core, &server);

    let settings: RoomSettings = value(call(
        &core,
        Call::RoomSettings {
            room_id: ROOM.into(),
        },
    ));
    assert_eq!(settings.join_rule, JoinRule::Public);
    assert_eq!(settings.history_visibility, HistoryVisibility::Joined);
    assert_eq!(settings.topic.as_deref(), Some("Synthetic topic"));
    assert_eq!(settings.own_level.map(PowerLevel::get), Some(100));
    let level = |key| {
        settings
            .levels
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.level.get())
    };
    assert_eq!(level(LevelKey::PowerLevels), Some(100));
    assert_eq!(level(LevelKey::Name), Some(50));
    // No entry of its own: the state default.
    assert_eq!(level(LevelKey::Topic), Some(50));
    assert!(settings.may.power_levels);
    assert_eq!(settings.published, Some(false));
    assert_eq!(settings.local_aliases, Some(vec![ALIAS.to_owned()]));
    // No guest access event: the homeserver answers `M_NOT_FOUND`.
    assert!(!settings.guest_access);

    let members: RoomMembers = value(call(
        &core,
        Call::RoomMembers {
            room_id: ROOM.into(),
            filter: String::new(),
        },
    ));
    let listed: Vec<_> = members
        .members
        .iter()
        .map(|member| (member.user_id.as_str(), member.membership))
        .collect();
    assert_eq!(
        listed,
        [(USER, Membership::Join), (CONTACT, Membership::Invite)]
    );
    assert_eq!(
        server.state_requests("m.room.power_levels"),
        0,
        "synced levels are not asked again"
    );
}

#[test]
fn given_native_sliding_sync_then_room_settings_come_from_the_room_state() {
    settings_come_from_the_room_state(true);
}

/// GIVEN Sliding Sync that sent `ROOM` without the power levels, join rule,
/// history setting and topic, as a server does that first sent the room
/// before the list asked for them WHEN its settings and members are read
/// THEN the core asks the homeserver for those events and shows the room's
/// own values, not "could not be reached" or the defaults; a member's level
/// change starts from them. Guest access, which is off without an event, is
/// on as the homeserver answers.
#[test]
fn given_sliding_sync_without_the_later_state_then_the_homeserver_is_asked() {
    let server = Homeserver::start(
        (
            200,
            json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}),
        ),
        true,
        true,
    );
    server.withhold_later_state();
    server.settings_endpoints();
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    let _locked = LockOnDrop(&core);
    sign_in_to_room(&core, &server);
    server.state_answers(
        "m.room.guest_access",
        200,
        json!({"guest_access": "can_join"}),
    );

    let settings: RoomSettings = value(call(
        &core,
        Call::RoomSettings {
            room_id: ROOM.into(),
        },
    ));
    assert_eq!(settings.join_rule, JoinRule::Public);
    assert_eq!(settings.history_visibility, HistoryVisibility::Joined);
    assert_eq!(settings.topic.as_deref(), Some("Synthetic topic"));
    assert!(settings.guest_access);
    assert_eq!(settings.own_level.map(PowerLevel::get), Some(100));
    assert!(settings.may.power_levels);
    assert!(server.state_requests("m.room.power_levels") > 0);
    assert!(server.state_requests("m.room.guest_access") > 0);

    let members: RoomMembers = value(call(
        &core,
        Call::RoomMembers {
            room_id: ROOM.into(),
            filter: String::new(),
        },
    ));
    let own = members.members.iter().find(|member| member.own).unwrap();
    assert_eq!(own.level.map(PowerLevel::get), Some(100));
    let invited = members
        .members
        .iter()
        .find(|member| member.user_id == CONTACT)
        .unwrap();
    assert!(invited.can_change_level);

    server.state_events_accepted();
    assert_eq!(
        call(
            &core,
            Call::ChangeRoom {
                room_id: ROOM.into(),
                change: RoomChange::MemberLevel {
                    user_id: CONTACT.into(),
                    level: PowerLevel::new(50).unwrap(),
                },
            },
        ),
        Outcome::Ok(Value::Null)
    );
    let sent = server.sent_power_levels();
    assert_eq!(sent["users"][USER], 100, "the existing entries are kept");
    assert_eq!(sent["users"][CONTACT], 50);
    assert_eq!(sent["events"]["m.room.power_levels"], 100);
}

/// GIVEN Sliding Sync without `ROOM`'s power levels WHEN the homeserver
/// refuses them (`M_FORBIDDEN`) or answers content that is not power levels
/// THEN the settings and members are `rejected` or `unavailable`, never
/// the defaults a room without power levels has.
#[test]
fn given_unsynced_levels_that_cannot_be_read_then_settings_fail_closed() {
    let server = Homeserver::start(
        (
            200,
            json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}),
        ),
        true,
        true,
    );
    server.withhold_later_state();
    server.settings_endpoints();
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    let _locked = LockOnDrop(&core);
    sign_in_to_room(&core, &server);
    let settings = || {
        call(
            &core,
            Call::RoomSettings {
                room_id: ROOM.into(),
            },
        )
    };
    let members = || {
        call(
            &core,
            Call::RoomMembers {
                room_id: ROOM.into(),
                filter: String::new(),
            },
        )
    };

    server.state_answers(
        "m.room.power_levels",
        403,
        json!({"errcode": "M_FORBIDDEN", "error": "not allowed"}),
    );
    assert_eq!(settings(), Outcome::Error(ErrorCode::Rejected));
    assert_eq!(members(), Outcome::Error(ErrorCode::Rejected));

    server.state_answers("m.room.power_levels", 200, json!({"users": "not a map"}));
    assert_eq!(settings(), Outcome::Error(ErrorCode::Unavailable));
    assert_eq!(members(), Outcome::Error(ErrorCode::Unavailable));
}

#[test]
fn given_classic_sync_then_room_settings_come_from_the_room_state() {
    settings_come_from_the_room_state(false);
}

/// GIVEN an account without a display name, which a Matrix v1.19
/// homeserver answers with 404 `M_NOT_FOUND` WHEN the name is read THEN
/// there is none, not a refusal. Setting one sends it, a blank one deletes
/// the field, and a refusal is `rejected`. A 404 that is not `M_NOT_FOUND`,
/// from a server without the endpoint, is a refusal: the name is not known.
#[test]
fn given_no_display_name_then_none_is_read_and_one_can_be_set() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let core = core_with_room(&server, dir.path());
    let _locked = LockOnDrop(&core);

    server.display_name_answers(
        "GET",
        404,
        json!({"errcode": "M_NOT_FOUND", "error": "Profile field not found"}),
    );
    let read: DisplayName = value(call(&core, Call::DisplayName {}));
    assert_eq!(read.display_name, None);

    server.display_name_answers("PUT", 200, json!({}));
    let set: DisplayName = value(call(
        &core,
        Call::SetDisplayName {
            name: "  Alice  ".into(),
        },
    ));
    assert_eq!(set.display_name.as_deref(), Some("Alice"));
    assert_eq!(
        server.display_name_requests("PUT"),
        [json!({"displayname": "Alice"})]
    );

    server.display_name_answers("GET", 200, json!({"displayname": "Alice"}));
    let read: DisplayName = value(call(&core, Call::DisplayName {}));
    assert_eq!(read.display_name.as_deref(), Some("Alice"));

    // Matrix v1.16 and later remove a profile field with DELETE.
    server.display_name_answers("DELETE", 200, json!({}));
    let removed: DisplayName = value(call(&core, Call::SetDisplayName { name: " ".into() }));
    assert_eq!(removed.display_name, None);
    assert_eq!(server.display_name_requests("DELETE").len(), 1);

    server.display_name_answers(
        "PUT",
        403,
        json!({"errcode": "M_FORBIDDEN", "error": "Display names are managed elsewhere"}),
    );
    assert_eq!(
        call(&core, Call::SetDisplayName { name: "Bob".into() }),
        Outcome::Error(ErrorCode::Rejected)
    );

    server.display_name_answers(
        "GET",
        404,
        json!({"errcode": "M_UNRECOGNIZED", "error": "Unrecognized request"}),
    );
    assert_eq!(
        call(&core, Call::DisplayName {}),
        Outcome::Error(ErrorCode::Rejected)
    );
}

/// Discovery that fails is an error, not an absence. The SDK discovers the
/// server's versions before logging in (and caches them for sync), so a
/// failure stops sign-in: no login, and neither sync mode is tried.
#[test]
fn given_capability_discovery_fails_then_sign_in_fails_without_any_sync() {
    let server = Homeserver::start(
        (
            200,
            json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}),
        ),
        true,
        false,
    );
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    sign_in(&core, &server.uri());
    wait_for("sign-in failure", || {
        core.session().account_status().state == AccountState::SignInFailed
    });
    assert!(server.requests_to("/_matrix/client/versions") > 0);
    assert_eq!(server.requests_to("/_matrix/client/v3/login"), 0);
    assert_eq!(
        server.requests_to("/_matrix/client/v3/sync"),
        0,
        "no classic fallback"
    );
    assert_eq!(
        server.requests_to("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"),
        0
    );
    // A client was built, so the lock is conservative.
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// Holds the prompt open until released, like a user still typing.
struct Held {
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
    asked: AtomicUsize,
}

impl CredentialSource for Held {
    fn ask(&self, _host: &str) -> Result<Credentials, PromptError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
        Ok(Credentials {
            username: Zeroizing::new("alice".into()),
            password: Zeroizing::new(PASSWORD.into()),
        })
    }
}

/// A lock while the prompt is open abandons that sign-in: the status returns
/// to signed out, no login is sent, and after unlocking a new sign-in prompts
/// again instead of reporting the old attempt as still running.
#[test]
fn given_a_lock_while_the_prompt_is_open_then_the_attempt_is_abandoned_cleanly() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let held = Arc::new(Held {
        released: std::sync::Mutex::new(false),
        wake: std::sync::Condvar::new(),
        asked: AtomicUsize::new(0),
    });
    let core = make_core(dir.path(), held.clone());
    assert_eq!(sign_in(&core, &server.uri()).state, AccountState::SigningIn);
    wait_for("the prompt", || held.asked.load(Ordering::SeqCst) == 1);

    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::Locked, "no client ran yet");
    *held.released.lock().unwrap() = true;
    held.wake.notify_all();
    wait_for("the attempt to end", || {
        core.session().account_status().state == AccountState::SignedOut
    });
    assert_eq!(server.requests_to("/_matrix/client/v3/login"), 0);

    let state: StoreState = value(call(&core, Call::UnlockSession {}));
    assert_eq!(state, StoreState::Unlocking);
    wait_for("the unlock", || {
        core.session().state() == StoreState::Protected
    });
    assert_eq!(sign_in(&core, &server.uri()).state, AccountState::SigningIn);
    wait_for("a new prompt", || held.asked.load(Ordering::SeqCst) == 2);
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// A lock and unlock while the prompt is open still abandon that attempt:
/// credentials typed into a prompt opened before the lock are never used.
#[test]
fn given_a_lock_and_unlock_while_the_prompt_is_open_then_the_old_attempt_never_signs_in() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let held = Arc::new(Held {
        released: std::sync::Mutex::new(false),
        wake: std::sync::Condvar::new(),
        asked: AtomicUsize::new(0),
    });
    let core = make_core(dir.path(), held.clone());
    assert_eq!(sign_in(&core, &server.uri()).state, AccountState::SigningIn);
    wait_for("the prompt", || held.asked.load(Ordering::SeqCst) == 1);

    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::Locked);
    let state: StoreState = value(call(&core, Call::UnlockSession {}));
    assert_eq!(state, StoreState::Unlocking);
    wait_for("the unlock", || {
        core.session().state() == StoreState::Protected
    });
    *held.released.lock().unwrap() = true;
    held.wake.notify_all();
    wait_for("the attempt to end", || {
        core.session().account_status().state == AccountState::SignedOut
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        server.requests_to("/_matrix/client/v3/login"),
        0,
        "old attempt never logs in"
    );
    assert_eq!(
        core.session().account_status().state,
        AccountState::SignedOut
    );
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::Locked, "no client ever ran");
}

fn signed_in_core(server: &Homeserver, dir: &Path) -> Core {
    let core = make_core(dir, Fixed::new(Ok(())));
    sign_in(&core, &server.uri());
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    core
}

/// Sign-out revokes the session and deletes the key at once; the store files
/// stay, unreadable, until the next start wipes them before anything opens,
/// and that start is a fresh, signed-out profile with nothing of the old
/// account left.
#[test]
fn given_sign_out_then_the_session_is_revoked_and_the_profile_is_wiped() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    {
        let core = signed_in_core(&server, dir.path());
        let state: StoreState = value(call(&core, Call::SignOut {}));
        assert_eq!(state, StoreState::RestartRequired);
        assert_eq!(server.requests_to("/_matrix/client/v3/logout"), 1);
        let status = core.session().account_status();
        assert_eq!(
            (status.state, status.error),
            (AccountState::SignedOut, None)
        );
        assert!(!dir.path().join("store-key.v2").exists(), "key deleted");
        assert!(
            dir.path().join("signed-out").exists() && dir.path().join("store").exists(),
            "store files are left for the next start"
        );
        assert_eq!(
            call(&core, Call::RoomList {}),
            Outcome::Error(ErrorCode::Locked)
        );
        let state: StoreState = value(call(&core, Call::UnlockSession {}));
        assert_eq!(state, StoreState::RestartRequired);
    }
    assert!(files_contain(dir.path(), TOKEN.as_bytes()).is_empty());
    // Stands in for any old store file; the next start must remove it.
    fs::write(dir.path().join("store").join("left-behind"), b"x").unwrap();

    let never = Fixed::new(Err(PromptError::Failed));
    let core = make_core(dir.path(), never.clone());
    assert_eq!(
        core.session().state(),
        StoreState::Protected,
        "fresh profile"
    );
    assert_eq!(
        core.session().account_status().state,
        AccountState::SignedOut
    );
    assert!(!dir.path().join("signed-out").exists(), "wipe finished");
    assert!(
        !dir.path().join("store").join("left-behind").exists(),
        "the old store directory was wiped"
    );
    assert_eq!(never.asked.load(Ordering::SeqCst), 0);
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(
        state,
        StoreState::Locked,
        "no account restored, no client ran"
    );
}

/// When the server cannot revoke, sign-out still completes locally and says
/// the server session may remain.
#[test]
fn given_the_server_cannot_revoke_then_sign_out_still_wipes_and_warns() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    server.fail_logout();
    let state: StoreState = value(call(&core, Call::SignOut {}));
    assert_eq!(state, StoreState::RestartRequired);
    assert_eq!(
        core.session().account_status().error,
        Some(SignInError::SessionNotRevoked)
    );
    assert!(
        !dir.path().join("store-key.v2").exists(),
        "key deleted anyway"
    );
}

/// A UI connection that sends nothing until its sender drops, then closes.
struct QuietUi(std::sync::mpsc::Receiver<()>);

impl std::io::Read for QuietUi {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        let _ = self.0.recv();
        Ok(0)
    }
}

/// Locks the session when dropped, also when the test fails first. Locking
/// stops the sync loop, which would otherwise go on polling the mock server
/// after wiremock hands it to another test.
struct LockOnDrop<'a>(&'a Core);

impl Drop for LockOnDrop<'_> {
    fn drop(&mut self) {
        self.0.session().lock(LOCK_TIMEOUT, || {});
    }
}

/// GIVEN a signed-in account and a connected UI WHEN the homeserver ends the
/// session (it refuses the token without soft logout, and again when asked
/// once more) THEN the core deletes the key at once, without waiting for the
/// next idle check and without asking the homeserver to log out, and ends
/// `restart_required` saying why; and the next start wipes the profile and
/// opens a new, empty one with no warning.
#[test]
fn given_the_homeserver_ends_the_session_then_the_profile_is_deleted_and_the_next_start_is_fresh() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    {
        let core = signed_in_core(&server, dir.path());
        let _locked = LockOnDrop(&core);
        wait_for("the first sync", || {
            server.requests_to("/_matrix/client/v3/sync") > 0
        });
        std::thread::scope(|scope| {
            // Inside the scope, so a failing assertion drops the sender and
            // the UI closes before the scope waits for it.
            let (close, quiet) = std::sync::mpsc::channel::<()>();
            let serving = scope.spawn(|| serve(&core, QuietUi(quiet), std::io::sink()));
            let refused = Instant::now();
            server.refuse_token(false, true);
            wait_for("the session to end", || {
                core.session().account_status().error == Some(SignInError::SessionEnded)
            });
            assert!(refused.elapsed() < IDLE_CHECK / 2, "woken, not polled");
            drop(close);
            assert_eq!(serving.join().unwrap(), ServeEnd::UiClosed);
        });
        let status = core.session().account_status();
        assert_eq!(
            (status.state, status.user_id, status.error),
            (
                AccountState::SignedOut,
                None,
                Some(SignInError::SessionEnded)
            )
        );
        assert_eq!(core.session().state(), StoreState::RestartRequired);
        assert!(!core.session().has_ended(), "finished once");
        assert!(!dir.path().join("store-key.v2").exists(), "key deleted");
        assert!(dir.path().join("signed-out").exists());
        assert_eq!(
            server.requests_to("/_matrix/client/v3/logout"),
            0,
            "nothing is left on the homeserver to revoke"
        );
        assert!(
            server.requests_to("/_matrix/client/v3/account/whoami") >= 2,
            "confirmed in two rounds"
        );
        assert_eq!(
            call(&core, Call::RoomList {}),
            Outcome::Error(ErrorCode::Locked)
        );
    }

    let never = Fixed::new(Err(PromptError::Failed));
    let core = make_core(dir.path(), never.clone());
    assert_eq!(
        core.session().state(),
        StoreState::Protected,
        "fresh profile"
    );
    let status = core.session().account_status();
    assert_eq!(
        (status.state, status.error),
        (AccountState::SignedOut, None),
        "the homeserver ended the session: nothing may stay active there"
    );
    assert!(!dir.path().join("signed-out").exists(), "wipe finished");
    assert_eq!(never.asked.load(Ordering::SeqCst), 0);
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::Locked, "no account restored");
}

/// GIVEN a signed-in account WHEN the homeserver asks for a soft logout (the
/// account and device still exist) THEN nothing is deleted: sync fails and
/// says so.
#[test]
fn given_a_soft_logout_then_the_profile_is_kept() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    let _locked = LockOnDrop(&core);
    server.refuse_token(true, true);
    wait_for("a refused round", || {
        core.session().account_status().error == Some(SignInError::SyncFailed)
    });
    let rounds = server.requests_to("/_matrix/client/v3/sync");
    wait_for("another refused round", || {
        server.requests_to("/_matrix/client/v3/sync") > rounds
    });
    assert!(!core.session().has_ended());
    assert!(!core.finish_ended_session());
    assert!(dir.path().join("store-key.v2").exists());
    assert_eq!(core.session().state(), StoreState::Protected);
    assert_eq!(
        core.session().account_status().state,
        AccountState::SignedIn
    );
}

/// GIVEN a sync round refused as if the session ended WHEN a second request
/// is still accepted THEN nothing is deleted: one stray answer is not enough.
#[test]
fn given_a_refusal_that_a_second_request_does_not_confirm_then_the_profile_is_kept() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    let _locked = LockOnDrop(&core);
    server.refuse_token(false, false);
    wait_for("the second request", || {
        server.requests_to("/_matrix/client/v3/account/whoami") > 0
    });
    let rounds = server.requests_to("/_matrix/client/v3/sync");
    wait_for("another refused round", || {
        server.requests_to("/_matrix/client/v3/sync") > rounds
    });
    assert!(!core.session().has_ended());
    assert!(!core.finish_ended_session());
    assert!(dir.path().join("store-key.v2").exists());
    assert_eq!(core.session().state(), StoreState::Protected);
    assert_eq!(
        core.session().account_status().error,
        Some(SignInError::SyncFailed)
    );
}

/// GIVEN one sync round in which the homeserver ends the session, confirmed
/// by `whoami`, WHEN the next round is accepted THEN nothing is deleted: the
/// end must hold for two rounds in a row.
#[test]
fn given_one_round_that_ends_the_session_then_a_good_round_keeps_the_profile() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    let _locked = LockOnDrop(&core);
    wait_for("the first sync", || {
        server.requests_to("/_matrix/client/v3/sync") > 0
    });
    server.refuse_token_once();
    wait_for("the refused round", || {
        server.requests_to("/_matrix/client/v3/account/whoami") > 0
    });
    let rounds = server.requests_to("/_matrix/client/v3/sync");
    wait_for("two more rounds", || {
        server.requests_to("/_matrix/client/v3/sync") > rounds + 1
    });
    assert!(!core.session().has_ended());
    assert!(!core.finish_ended_session());
    assert!(dir.path().join("store-key.v2").exists());
    let status = core.session().account_status();
    assert_eq!((status.state, status.error), (AccountState::SignedIn, None));
}

/// GIVEN a password session that holds a refresh token WHEN the homeserver
/// refuses the access token and then the refresh token THEN the session has
/// ended, as for a session without one.
#[test]
fn given_a_refused_refresh_token_then_the_session_ends() {
    let server = Homeserver::start(
        (
            200,
            json!({"user_id":USER,"access_token":TOKEN,"refresh_token":"synthetic_refresh_token","device_id":"PERSEPHONE1"}),
        ),
        false,
        true,
    );
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    let _locked = LockOnDrop(&core);
    server.refuse_refresh();
    server.refuse_token(false, true);
    wait_for("the session to end", || core.session().has_ended());
    assert!(server.requests_to("/_matrix/client/v3/refresh") > 0);
}

#[test]
fn given_no_account_then_sign_out_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    let state: StoreState = value(call(&core, Call::SignOut {}));
    assert_eq!(state, StoreState::Protected);
    assert!(dir.path().join("store-key.v2").exists());
}

/// GIVEN every lease in use WHEN signing out THEN the sign-out waits for one;
/// if none frees in time nothing is signed out and the call says so, and once
/// a lease frees the sign-out completes. (The first call waits not at all:
/// the sync loop frees and retakes its own lease between rounds.)
#[test]
fn given_a_saturated_session_then_sign_out_waits_or_reports_that_it_did_not_start() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    let mut leases = Vec::new();
    while let Ok(lease) = core.session().admit() {
        leases.push(lease);
    }
    assert_eq!(
        core.session().sign_out(Duration::ZERO, || {}),
        Err(AdmissionError::Saturated)
    );
    assert_eq!(server.requests_to("/_matrix/client/v3/logout"), 0);
    assert!(dir.path().join("store-key.v2").exists(), "nothing deleted");
    assert_eq!(
        core.session().account_status().state,
        AccountState::SignedIn
    );

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(200));
            leases.clear();
        });
        assert_eq!(
            core.session().sign_out(Duration::from_secs(10), || {}),
            Ok(StoreState::RestartRequired)
        );
    });
    assert_eq!(server.requests_to("/_matrix/client/v3/logout"), 1);
    assert!(!dir.path().join("store-key.v2").exists());
}

/// GIVEN a signed-in profile whose sign-out stopped after recording its
/// intent (as if the process ended during the logout request) WHEN
/// Persephone starts again THEN the account is not restored: the profile is
/// wiped, nothing asks for credentials, and the page is told the homeserver
/// may still hold the session.
#[test]
fn given_a_sign_out_interrupted_during_logout_then_the_next_start_wipes_and_warns() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    {
        let core = signed_in_core(&server, dir.path());
        persephone_core::store::begin_sign_out(dir.path()).unwrap();
        let state: StoreState = value(call(&core, Call::LockSession {}));
        assert_eq!(state, StoreState::RestartRequired);
    }
    let never = Fixed::new(Err(PromptError::Failed));
    let core = make_core(dir.path(), never.clone());
    assert_eq!(
        core.session().state(),
        StoreState::Protected,
        "fresh profile"
    );
    let status = core.session().account_status();
    assert_eq!(
        (status.state, status.error),
        (
            AccountState::SignedOut,
            Some(SignInError::SessionNotRevoked)
        )
    );
    assert_eq!(never.asked.load(Ordering::SeqCst), 0);
    assert_eq!(server.requests_to("/_matrix/client/v3/logout"), 0);
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(
        state,
        StoreState::Locked,
        "no account restored, no client ran"
    );
}

/// GIVEN a homeserver that is slow to answer logout WHEN signing out THEN the
/// sign-out intent is on disk while the request is still in flight (with the
/// key not yet deleted), so ending the process then cannot restore the account.
#[test]
fn given_a_slow_logout_then_the_sign_out_intent_is_recorded_before_the_request_ends() {
    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    server.slow_logout(Duration::from_secs(2));
    std::thread::scope(|scope| {
        let signing_out = scope.spawn(|| core.session().sign_out(Duration::from_secs(10), || {}));
        wait_for("the logout request", || {
            server.requests_to("/_matrix/client/v3/logout") == 1
        });
        assert!(
            dir.path().join("signed-out").exists(),
            "intent recorded before the answer"
        );
        assert!(
            dir.path().join("store-key.v2").exists(),
            "key deleted only after the answer"
        );
        assert_eq!(signing_out.join().unwrap(), Ok(StoreState::RestartRequired));
    });
    assert!(!dir.path().join("store-key.v2").exists());
}

/// GIVEN a signed-in core whose UI connection ended (a failed window) WHEN
/// the core is reopened and a new UI connects THEN the new page sees the same
/// signed-in account and its rooms, with no second sign-in and no prompt.
#[test]
fn given_a_replaced_ui_then_the_signed_in_session_carries_on_for_the_new_page() {
    use persephone_contracts::AppStatus;
    use persephone_core::ServeEnd;
    use persephone_core_protocol::{CoreMessage, Reply, receive, send};
    use persephone_media_protocol::ResourceBudgets;
    use std::io::Cursor;

    let server = Homeserver::signed_in();
    let dir = TempDir::new().unwrap();
    let source = Fixed::new(Ok(()));
    let core = make_core(dir.path(), source.clone());
    sign_in(&core, &server.uri());
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    wait_for("the first sync", || {
        value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .first()
            .is_some_and(|room| room.display_name.is_some())
    });
    // One UI connection: its requests, then its replies once it closes.
    let connect = |calls: Vec<Call>| -> Vec<Reply> {
        let mut wire = Vec::new();
        for (n, call) in calls.into_iter().enumerate() {
            send(
                &mut wire,
                &Request {
                    id: n as u32 + 1,
                    call,
                },
            )
            .unwrap();
        }
        let mut output = Vec::new();
        assert_eq!(
            persephone_core::serve(&core, Cursor::new(wire), &mut output),
            ServeEnd::UiClosed
        );
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(output);
        std::iter::from_fn(|| receive::<_, CoreMessage>(&mut reader, &budgets).ok())
            .filter_map(|message| match message {
                CoreMessage::Reply(reply) => Some(reply),
                CoreMessage::Event(_) => None,
            })
            .collect()
    };

    assert!(
        !connect(vec![Call::AppStatus {}]).is_empty(),
        "the first UI"
    );
    core.reopen();
    let mut replies = connect(vec![Call::AppStatus {}, Call::RoomList {}]).into_iter();
    let status: AppStatus = value(replies.next().expect("status reply").outcome);
    assert_eq!(status.store, StoreState::Protected);
    assert_eq!(status.account.state, AccountState::SignedIn);
    assert_eq!(status.account.user_id.as_deref(), Some(USER));
    let rooms: RoomList = value(replies.next().expect("room list reply").outcome);
    assert_eq!(rooms.rooms[0].room_id, ROOM, "the account's rooms");
    assert_eq!(
        server.requests_to("/_matrix/client/v3/login"),
        1,
        "no second sign-in"
    );
    assert_eq!(source.asked.load(Ordering::SeqCst), 1, "no second prompt");
    // Stop the sync loop: wiremock reuses this server's address for later
    // tests, which must not see this client's requests.
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

fn security(core: &Core) -> SecurityStatus {
    value(call(core, Call::SecurityStatus {}))
}

/// GIVEN a signed-in account the homeserver says has no identity WHEN the
/// page asks to set up encryption THEN the core asks for the password once in
/// its own prompt, answers the homeserver's authentication round with it,
/// sets up recovery and shows the recovery key only through the prompt
/// source: never in a reply.
#[test]
fn given_no_identity_when_setting_up_encryption_then_the_password_is_asked_once_and_the_key_stays_in_the_core()
 {
    let server = Homeserver::signed_in();
    server.encryption_endpoints();
    let dir = TempDir::new().unwrap();
    let source = Fixed::new(Ok(()));
    let core = make_core(dir.path(), source.clone());
    sign_in(&core, &server.uri());
    wait_for("the account to have no identity", || {
        security(&core).identity == IdentityState::None
    });

    let reply: SecurityStatus = value(call(&core, Call::SetUpEncryption {}));
    assert_ne!(
        reply.operation,
        SecurityOperation::Idle,
        "started at once: {reply:?}"
    );
    wait_for("set-up to finish", || {
        let status = security(&core);
        status.operation == SecurityOperation::Idle && status.identity != IdentityState::None
    });
    let status = security(&core);
    assert_eq!(status.error, None, "{status:?}");
    assert_ne!(status.identity, IdentityState::None);
    assert_eq!(source.secrets_asked.load(Ordering::SeqCst), 1, "asked once");

    let uploads = server.bodies_to("/_matrix/client/v3/keys/device_signing/upload");
    assert!(
        uploads.len() >= 2,
        "an authentication round: {}",
        uploads.len()
    );
    assert!(
        uploads[0].get("auth").is_none(),
        "first upload asks how to authenticate"
    );
    assert_eq!(uploads.last().unwrap()["auth"]["session"], "uia-session-1");

    // The key window has its own thread, which may still be starting.
    wait_for("the recovery key to be shown", || {
        source.shown.lock().unwrap().is_some()
    });
    let key = source.shown.lock().unwrap().clone().unwrap();
    assert!(key.len() >= 40, "a recovery key, not an empty string");
    let replies = serde_json::to_string(&(
        call(&core, Call::AppStatus {}),
        call(&core, Call::SecurityStatus {}),
    ))
    .unwrap();
    assert!(
        !replies.contains(&key),
        "the recovery key never crosses to the UI"
    );
    // Set-up is not offered again once an identity exists.
    let again: SecurityStatus = value(call(&core, Call::SetUpEncryption {}));
    assert_eq!(again.operation, SecurityOperation::Idle);
    assert_eq!(source.secrets_asked.load(Ordering::SeqCst), 1);
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(
        state,
        StoreState::RestartRequired,
        "the security thread is joined"
    );
    assert_eq!(
        security(&core),
        SecurityStatus::unknown(),
        "nothing kept after the lock"
    );
    // With the stores closed, no profile file holds the key in plain text.
    assert!(files_contain(dir.path(), key.as_bytes()).is_empty());
}

/// GIVEN a recovery-key window that fails at once WHEN set-up succeeds THEN
/// the status keeps saying the key was not shown: set-up's own result, which
/// is written first, does not replace it.
#[test]
fn given_a_failing_key_window_then_set_up_reports_the_key_was_not_shown() {
    let server = Homeserver::signed_in();
    server.encryption_endpoints();
    let dir = TempDir::new().unwrap();
    let source = Fixed::failing_window();
    let core = make_core(dir.path(), source.clone());
    sign_in(&core, &server.uri());
    wait_for("the account to have no identity", || {
        security(&core).identity == IdentityState::None
    });
    let _: SecurityStatus = value(call(&core, Call::SetUpEncryption {}));
    wait_for("the failure to be reported", || {
        security(&core).error == Some(SecurityError::RecoveryKeyNotShown)
    });
    // Later refreshes keep it.
    std::thread::sleep(Duration::from_secs(3));
    let status = security(&core);
    assert_eq!(status.error, Some(SecurityError::RecoveryKeyNotShown));
    assert_eq!(status.operation, SecurityOperation::Idle);
    assert!(source.shown.lock().unwrap().is_none());
    // Joins the sync and security threads: a core left running keeps polling
    // this mock server's port, which wiremock hands to a later test.
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN set-up that has reached the backup request WHEN the session is
/// locked before set-up ends THEN the lock waits for the step, and the new
/// recovery key, which recovery is already enabled with, is still shown.
#[test]
fn given_a_lock_during_set_up_then_the_new_recovery_key_is_still_shown() {
    let server = Homeserver::signed_in();
    server.runtime.block_on(
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/room_keys/version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"version": "1"}))
                    .set_delay(Duration::from_secs(2)),
            )
            .with_priority(1)
            .mount(&server.server),
    );
    server.encryption_endpoints();
    let dir = TempDir::new().unwrap();
    let source = Fixed::new(Ok(()));
    let core = make_core(dir.path(), source.clone());
    sign_in(&core, &server.uri());
    wait_for("the account to have no identity", || {
        security(&core).identity == IdentityState::None
    });
    let _: SecurityStatus = value(call(&core, Call::SetUpEncryption {}));
    wait_for("the backup request", || {
        server
            .bodies_to("/_matrix/client/v3/room_keys/version")
            .iter()
            .any(Value::is_object)
    });
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
    wait_for("the recovery key to be shown", || {
        source.shown.lock().unwrap().is_some()
    });
    assert_eq!(
        security(&core),
        SecurityStatus::unknown(),
        "nothing kept after the lock"
    );
}

/// GIVEN a prompt the user cancels, or a password the homeserver refuses
/// WHEN setting up encryption THEN nothing is uploaded with a password (for
/// a cancel) and the status says why, with the account still unset.
#[test]
fn given_a_cancelled_prompt_or_a_wrong_password_then_set_up_reports_it() {
    for (secret, expected, authenticated) in [
        (Err(PromptError::Cancelled), SecurityError::Cancelled, false),
        (
            Ok("wrong-password".to_owned()),
            SecurityError::InvalidPassword,
            true,
        ),
    ] {
        let server = Homeserver::signed_in();
        server.encryption_endpoints();
        let dir = TempDir::new().unwrap();
        let source = Fixed::with_secret(Ok(()), secret);
        let core = make_core(dir.path(), source.clone());
        sign_in(&core, &server.uri());
        wait_for("no identity", || {
            security(&core).identity == IdentityState::None
        });
        call(&core, Call::SetUpEncryption {});
        wait_for("the failure", || security(&core).error.is_some());
        let status = security(&core);
        assert_eq!(status.error, Some(expected));
        assert_eq!(status.operation, SecurityOperation::Idle);
        assert!(
            source.shown.lock().unwrap().is_none(),
            "no key for a failed set-up"
        );
        let with_auth = server
            .bodies_to("/_matrix/client/v3/keys/device_signing/upload")
            .into_iter()
            .filter(|body| body.get("auth").is_some())
            .count();
        assert_eq!(with_auth > 0, authenticated, "{expected:?}");
        assert_eq!(
            status.identity,
            IdentityState::None,
            "nothing reached the homeserver, so set-up can be retried"
        );
        let state: StoreState = value(call(&core, Call::LockSession {}));
        assert_eq!(state, StoreState::RestartRequired);
    }
}

/// GIVEN a set-up refused for a wrong password WHEN the user tries again with
/// the right one THEN set-up runs again, uploads the identity and shows a key.
#[test]
fn given_a_wrong_password_then_set_up_can_be_retried_and_succeeds() {
    let server = Homeserver::signed_in();
    server.encryption_endpoints();
    let dir = TempDir::new().unwrap();
    let source = Fixed::with_secret(Ok(()), Ok("wrong-password".to_owned()));
    source.secrets.lock().unwrap().push(Ok(PASSWORD.to_owned()));
    let core = make_core(dir.path(), source.clone());
    sign_in(&core, &server.uri());
    wait_for("no identity", || {
        security(&core).identity == IdentityState::None
    });
    call(&core, Call::SetUpEncryption {});
    wait_for("the refusal", || security(&core).error.is_some());
    assert_eq!(security(&core).error, Some(SecurityError::InvalidPassword));
    assert_eq!(security(&core).identity, IdentityState::None);

    let again: SecurityStatus = value(call(&core, Call::SetUpEncryption {}));
    assert_ne!(again.operation, SecurityOperation::Idle, "offered again");
    wait_for("set-up to finish", || {
        let status = security(&core);
        status.operation == SecurityOperation::Idle && status.identity != IdentityState::None
    });
    assert_eq!(security(&core).error, None);
    wait_for("the recovery key", || {
        source.shown.lock().unwrap().is_some()
    });
    assert_eq!(source.secrets_asked.load(Ordering::SeqCst), 2);
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN a homeserver that fails the key backup once WHEN encryption is set
/// up THEN the identity exists but recovery does not, and setting up recovery
/// alone (no password) then succeeds and shows a key.
#[test]
fn given_recovery_fails_after_cross_signing_then_recovery_alone_can_be_set_up() {
    let server = Homeserver::signed_in();
    server.encryption_endpoints();
    server.runtime.block_on(
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/room_keys/version"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(json!({"errcode": "M_UNKNOWN", "error": "down"})),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server.server),
    );
    let dir = TempDir::new().unwrap();
    let source = Fixed::new(Ok(()));
    let core = make_core(dir.path(), source.clone());
    sign_in(&core, &server.uri());
    wait_for("no identity", || {
        security(&core).identity == IdentityState::None
    });
    call(&core, Call::SetUpEncryption {});
    wait_for("the recovery failure", || security(&core).error.is_some());
    wait_for("the identity", || {
        security(&core).identity != IdentityState::None
    });
    let status = security(&core);
    assert_eq!(
        status.recovery,
        persephone_contracts::RecoveryState::Disabled,
        "{status:?}"
    );
    assert!(source.shown.lock().unwrap().is_none(), "no key yet");
    // Set-up as a whole is not offered again over the identity.
    let refused: SecurityStatus = value(call(&core, Call::SetUpEncryption {}));
    assert_eq!(refused.operation, SecurityOperation::Idle);

    let started: SecurityStatus = value(call(&core, Call::SetUpRecovery {}));
    assert_eq!(started.operation, SecurityOperation::Working);
    wait_for("the recovery set-up to end", || {
        security(&core).operation == SecurityOperation::Idle
    });
    // The mock never syncs account data back, so the SDK cannot report
    // recovery as enabled here (the live runs do); the second backup request
    // and the shown key show that recovery was set up.
    assert_eq!(
        server
            .bodies_to("/_matrix/client/v3/room_keys/version")
            .iter()
            .filter(|body| body.is_object())
            .count(),
        2,
        "the failed backup, then the retry"
    );
    wait_for("the recovery key", || {
        source.shown.lock().unwrap().is_some()
    });
    assert_eq!(security(&core).error, None);
    assert_eq!(
        source.secrets_asked.load(Ordering::SeqCst),
        1,
        "no second password"
    );
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// Signs in at once and holds the secret prompt open until released, like a
/// user still typing the password.
struct HeldSecret {
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
    asked: AtomicUsize,
}

impl CredentialSource for HeldSecret {
    fn ask(&self, _host: &str) -> Result<Credentials, PromptError> {
        Ok(Credentials {
            username: Zeroizing::new("alice".into()),
            password: Zeroizing::new(PASSWORD.into()),
        })
    }

    fn ask_secret(&self, _prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
        Ok(Zeroizing::new(PASSWORD.into()))
    }
}

/// GIVEN a password prompt left open WHEN the session locks
/// THEN the answer is dropped: nothing is set up with it.
#[test]
fn given_a_lock_while_the_password_prompt_is_open_then_the_answer_is_dropped() {
    let server = Homeserver::signed_in();
    server.encryption_endpoints();
    let dir = TempDir::new().unwrap();
    let held = Arc::new(HeldSecret {
        released: std::sync::Mutex::new(false),
        wake: std::sync::Condvar::new(),
        asked: AtomicUsize::new(0),
    });
    let core = Core::default()
        .with_credentials(held.clone())
        .with_session(common::open_session(dir.path()));
    sign_in(&core, &server.uri());
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    wait_for("no identity", || {
        security(&core).identity == IdentityState::None
    });
    call(&core, Call::SetUpEncryption {});
    wait_for("the prompt", || held.asked.load(Ordering::SeqCst) == 1);
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
    *held.released.lock().unwrap() = true;
    held.wake.notify_all();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        server
            .bodies_to("/_matrix/client/v3/keys/device_signing/upload")
            .iter()
            .all(|body| body.get("auth").is_none()),
        "the late password is never sent"
    );
    assert_eq!(security(&core), SecurityStatus::unknown());
}

fn pending(core: &Core, room_id: &str) -> Vec<PendingMessage> {
    value(call(
        core,
        Call::PendingMessages {
            room_id: room_id.into(),
        },
    ))
}

fn send(core: &Core, room_id: &str, body: &str) -> Outcome {
    call(
        core,
        Call::SendMessage {
            room_id: room_id.into(),
            body: body.into(),
            reply_to: None,
        },
    )
}

/// GIVEN a device that is not verified WHEN sending in an encrypted room THEN
/// the message fails as `device_not_verified` and no request to send anything,
/// encrypted or plain, reaches the homeserver.
#[test]
fn given_an_unverified_device_then_an_encrypted_room_send_fails_without_any_send_request() {
    let server = Homeserver::signed_in();
    server.sending_endpoints(200);
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    // Before the first sync the room is unknown and the send fails as
    // `not_joined`; this test is about the joined, encrypted room.
    wait_for("the encrypted room", || {
        value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .iter()
            .any(|room| room.room_id == ROOM)
    });
    let queued: PendingMessage = value(send(&core, ROOM, "secret text"));
    assert_eq!(queued.state, PendingState::Queued);
    wait_for("the send to end", || {
        pending(&core, ROOM)[0].state == PendingState::Failed
    });
    assert_eq!(
        pending(&core, ROOM)[0].failure,
        Some(SendFailure::DeviceNotVerified)
    );
    assert!(
        server.sends().is_empty(),
        "nothing sent: {:?}",
        server.sends()
    );
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(
        state,
        StoreState::RestartRequired,
        "the sending thread is joined"
    );
    assert!(
        pending(&core, ROOM).is_empty(),
        "unsent messages are dropped"
    );
}

/// GIVEN a room that is not encrypted WHEN sending without consent THEN
/// nothing is sent and the message asks for confirmation; after consent a
/// retry sends it as plain text with the same transaction ID, and consent is
/// remembered for the next message.
#[test]
fn given_an_unencrypted_room_then_sending_needs_consent_once_and_keeps_the_transaction_id() {
    let server = Homeserver::signed_in();
    server.sending_endpoints(200);
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    wait_for("the plain room", || {
        value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .iter()
            .any(|room| room.room_id == PLAIN)
    });
    let first: PendingMessage = value(send(&core, PLAIN, "plain hello"));
    wait_for("the confirmation request", || {
        pending(&core, PLAIN)[0].state == PendingState::Failed
    });
    // The send checked the room, so the list now labels it.
    assert!(
        value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .iter()
            .any(|room| room.room_id == PLAIN && room.encryption == RoomEncryption::NotEncrypted)
    );
    assert_eq!(
        pending(&core, PLAIN)[0].failure,
        Some(SendFailure::ConfirmationRequired)
    );
    assert!(server.sends().is_empty(), "nothing sent before consent");

    assert_eq!(
        call(
            &core,
            Call::AllowUnencrypted {
                room_id: PLAIN.into()
            }
        ),
        Outcome::Ok(Value::Null)
    );
    assert_eq!(
        call(
            &core,
            Call::RetryMessage {
                local_id: first.local_id
            }
        ),
        Outcome::Ok(Value::Null)
    );
    wait_for("the send", || {
        pending(&core, PLAIN)[0].state == PendingState::Sent
    });
    let sent = &pending(&core, PLAIN)[0];
    assert_eq!(sent.event_id.as_deref(), Some("$sent"));
    let paths = server.sends();
    assert_eq!(paths.len(), 1);
    assert!(
        paths[0].contains("/send/m.room.message/"),
        "plain text: {}",
        paths[0]
    );

    value::<PendingMessage>(send(&core, PLAIN, "second"));
    wait_for("the second send", || {
        pending(&core, PLAIN)
            .iter()
            .filter(|m| m.state == PendingState::Sent)
            .count()
            == 2
    });
    assert_eq!(server.sends().len(), 2, "consent was remembered");
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN a homeserver that refuses a send WHEN the user retries THEN the same
/// transaction ID is sent again, so the homeserver can recognise it; a later
/// message in the room waits until the earlier one is resolved.
#[test]
fn given_a_refused_send_then_a_retry_reuses_its_transaction_id_and_later_messages_wait() {
    let server = Homeserver::signed_in();
    server.sending_endpoints(500);
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    assert_eq!(
        call(
            &core,
            Call::AllowUnencrypted {
                room_id: PLAIN.into()
            }
        ),
        Outcome::Ok(Value::Null)
    );
    wait_for("the plain room", || {
        value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .iter()
            .any(|room| room.room_id == PLAIN)
    });
    let first: PendingMessage = value(send(&core, PLAIN, "one"));
    let second: PendingMessage = value(send(&core, PLAIN, "two"));
    wait_for("the refusal", || {
        pending(&core, PLAIN)[0].state == PendingState::Failed
    });
    assert_eq!(
        pending(&core, PLAIN)[0].failure,
        Some(SendFailure::Rejected)
    );
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        pending(&core, PLAIN)[1].state,
        PendingState::Queued,
        "waits behind it"
    );
    assert_eq!(server.sends().len(), 1);

    server.accept_sends();
    call(
        &core,
        Call::RetryMessage {
            local_id: first.local_id,
        },
    );
    wait_for("both sent", || {
        pending(&core, PLAIN)
            .iter()
            .all(|m| m.state == PendingState::Sent)
    });
    let paths = server.sends();
    assert_eq!(paths.len(), 3);
    assert_eq!(paths[0], paths[1], "the retry repeats the transaction ID");
    assert_ne!(paths[1], paths[2], "the next message has its own");
    assert_eq!(
        call(
            &core,
            Call::DiscardMessage {
                local_id: second.local_id
            }
        ),
        Outcome::Ok(Value::Null)
    );
    assert_eq!(pending(&core, PLAIN).len(), 1);
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN bad input or no account WHEN sending THEN the core refuses before
/// queueing anything.
#[test]
fn given_bad_input_or_no_account_then_nothing_is_queued() {
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    assert_eq!(
        send(&core, ROOM, "x"),
        Outcome::Error(ErrorCode::Unavailable)
    );
    for (room, body) in [
        ("#alias:s", "x"),
        (ROOM, ""),
        (ROOM, "  "),
        (ROOM, "a\u{7}b"),
    ] {
        assert_eq!(
            send(&core, room, body),
            Outcome::Error(ErrorCode::InvalidArgument),
            "{room} {body:?}"
        );
    }
    assert_eq!(
        call(&core, Call::RetryMessage { local_id: 1 }),
        Outcome::Error(ErrorCode::NotFound)
    );
}

/// GIVEN an account without an identity and no verification WHEN the page
/// asks to verify, or acts on a flow THEN nothing starts and the flow ID is
/// checked: an empty or oversized one is refused, an unknown one ignored.
#[test]
fn given_no_flow_then_verification_actions_change_nothing() {
    let server = Homeserver::signed_in();
    server.encryption_endpoints();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    wait_for("no identity", || {
        security(&core).identity == IdentityState::None
    });
    let started: SecurityStatus = value(call(&core, Call::VerifyThisDevice {}));
    assert_eq!(started.verification, None, "nothing to verify against");
    for call_for in [
        |flow_id| Call::AcceptVerification { flow_id },
        |flow_id| Call::ConfirmVerification { flow_id },
        |flow_id| Call::CancelVerification { flow_id },
    ] {
        let status: SecurityStatus = value(call(&core, call_for("someone-else".into())));
        assert_eq!(status.verification, None);
        assert_eq!(status.error, None);
        for bad in [String::new(), "f".repeat(129)] {
            assert_eq!(
                call(&core, call_for(bad)),
                Outcome::Error(ErrorCode::InvalidArgument)
            );
        }
    }
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN a direct-message room with a contact who has no identity, and a
/// room still marked direct with them that a third person joined WHEN the
/// page lists rooms and asks about the contact THEN only the two-person room
/// names the contact, and their trust is `no_identity`; any other room has no
/// contact, and a bad room ID is refused.
#[test]
fn given_a_direct_room_then_its_contact_and_their_trust_are_shown() {
    let server = Homeserver::signed_in();
    server.direct_endpoints();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    let mut list = RoomList {
        rooms: Vec::new(),
        truncated: false,
    };
    wait_for("the direct room and its contact", || {
        list = value(call(&core, Call::RoomList {}));
        list.rooms
            .iter()
            .any(|room| room.room_id == DM && room.contact.is_some())
    });
    assert!(
        list.rooms.iter().any(|room| room.room_id == GROUP),
        "the group room is listed"
    );
    for room in &list.rooms {
        let expected = (room.room_id == DM).then(|| CONTACT.to_owned());
        assert_eq!(room.contact, expected, "{}", room.room_id);
    }
    let dm = list.rooms.iter().find(|room| room.room_id == DM).unwrap();
    assert_eq!(
        dm.display_name.as_deref(),
        Some(CONTACT),
        "an unnamed direct room is named after its contact"
    );
    let status: ContactStatus = value(call(&core, Call::ContactStatus { room_id: DM.into() }));
    assert_eq!(status.room_id, DM);
    assert_eq!(status.user_id, CONTACT);
    assert_eq!(status.trust, ContactTrust::NoIdentity);
    for other in [ROOM, GROUP] {
        assert_eq!(
            call(
                &core,
                Call::ContactStatus {
                    room_id: other.into()
                }
            ),
            Outcome::Error(ErrorCode::NotFound),
            "{other}"
        );
    }
    for bad in [String::new(), "dm".into(), format!("!{}", "x".repeat(255))] {
        assert_eq!(
            call(
                &core,
                Call::ContactStatus {
                    room_id: bad.clone()
                }
            ),
            Outcome::Error(ErrorCode::InvalidArgument),
            "{bad}"
        );
    }
    // Stops the sync loop: the mock server goes back to a shared pool.
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN a device that is not verified WHEN the page asks to verify the
/// contact of a direct-message room THEN nothing is queued and nothing is
/// sent to the room; a bad room ID is refused.
#[test]
fn given_an_unverified_device_then_verifying_a_contact_sends_nothing() {
    let server = Homeserver::signed_in();
    server.encryption_endpoints();
    server.direct_endpoints();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    wait_for("no identity", || {
        security(&core).identity == IdentityState::None
    });
    let status: SecurityStatus = value(call(&core, Call::VerifyContact { room_id: DM.into() }));
    assert_eq!(status.operation, SecurityOperation::Idle, "not queued");
    assert_eq!(status.verification, None);
    assert_eq!(status.error, None);
    assert_eq!(
        call(
            &core,
            Call::VerifyContact {
                room_id: "#alias:synthetic.test".into()
            }
        ),
        Outcome::Error(ErrorCode::InvalidArgument)
    );
    std::thread::sleep(Duration::from_millis(600));
    assert!(server.sends().is_empty(), "no request reached the room");
    assert_eq!(security(&core).verification, None);
    // Stops the sync loop: the mock server goes back to a shared pool.
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN a direct-message room whose contact's identity did not change WHEN
/// the page asks for the room's changed identities, or asks to accept one
/// THEN none is listed, accepting is `not_found` and asks nothing, and bad
/// room or user IDs are refused (ADR-0007).
#[test]
fn given_no_changed_identity_then_nothing_is_listed_or_accepted() {
    let server = Homeserver::signed_in();
    server.direct_endpoints();
    let dir = TempDir::new().unwrap();
    let core = signed_in_core(&server, dir.path());
    wait_for("the direct room", || {
        let list: RoomList = value(call(&core, Call::RoomList {}));
        list.rooms.iter().any(|room| room.room_id == DM)
    });
    let changes: RoomIdentityChanges =
        value(call(&core, Call::IdentityChanges { room_id: DM.into() }));
    assert_eq!(changes.room_id, DM);
    assert!(changes.changes.is_empty());
    assert!(!changes.truncated);
    assert_eq!(
        call(
            &core,
            Call::IdentityChanges {
                room_id: "!unknown:synthetic.test".into()
            }
        ),
        Outcome::Error(ErrorCode::NotFound)
    );
    assert_eq!(
        call(
            &core,
            Call::AcceptIdentity {
                room_id: DM.into(),
                user_id: CONTACT.into()
            }
        ),
        Outcome::Error(ErrorCode::NotFound),
        "nothing to accept"
    );
    assert_eq!(security(&core).operation, SecurityOperation::Idle);
    for (room_id, user_id) in [
        ("dm", CONTACT.to_owned()),
        (DM, "bob".to_owned()),
        (DM, format!("@{}:synthetic.test", "b".repeat(250))),
    ] {
        assert_eq!(
            call(
                &core,
                Call::AcceptIdentity {
                    room_id: room_id.into(),
                    user_id
                }
            ),
            Outcome::Error(ErrorCode::InvalidArgument),
            "{room_id}"
        );
    }
    // Stops the sync loop: the mock server goes back to a shared pool.
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// The synthetic 16 x 8 JPEG of the sandbox fixtures.
#[cfg(windows)]
const JPEG: &[u8] = include_bytes!("../../media-sandbox/tests/fixtures/solid-16x8-baseline.jpg");

#[cfg(windows)]
fn image_event(id: &str, content: Value) -> Value {
    json!({"type":"m.room.message","room_id":ROOM,"sender":USER,"event_id":id,"origin_server_ts":9,"content":content})
}

/// A core with the real contained worker in a temporary profile; the
/// worker binary is built by `tools/dev.py rust` before the tests.
#[cfg(windows)]
fn core_with_worker(dir: &Path) -> Core {
    use persephone_core::media::contained::ContainedWorker;
    let worker = std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(|deps| deps.parent())
        .unwrap()
        .join("persephone-media-worker.exe");
    assert!(
        worker.is_file(),
        "worker binary missing at {}",
        worker.display()
    );
    // One profile per core: a temporary profile is deleted when dropped, and
    // tests run at once in this process.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let profile = persephone_media_broker::AppContainerProfile::temporary(&format!(
        "Persephone.MediaWorker.AccountTest.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
    .unwrap();
    make_core(dir, Fixed::new(Ok(())))
        .with_image_worker(Arc::new(ContainedWorker::with_profile(worker, profile)))
}

/// GIVEN a signed-in account and `m.image` events (plain, encrypted with the
/// SDK's attachment encryptor, tampered, declared too large) WHEN the page
/// loads them THEN the plain and encrypted files are fetched through the
/// authenticated media API with the access token, decrypted, decoded in the
/// contained worker and kept under a handle; the tampered file fails its
/// hash check; the oversized one is refused without any download; and after
/// a lock every handle is gone.
#[cfg(windows)]
#[test]
fn given_image_events_then_load_image_fetches_decrypts_and_decodes_them() {
    use matrix_sdk_crypto::AttachmentEncryptor;
    use persephone_contracts::ImageView;
    use std::io::Read;

    let server = Homeserver::signed_in();
    let mut cursor = std::io::Cursor::new(JPEG.to_vec());
    let mut encryptor = AttachmentEncryptor::new(&mut cursor);
    let mut encrypted = Vec::new();
    encryptor.read_to_end(&mut encrypted).unwrap();
    let info = serde_json::to_value(encryptor.finish()).unwrap();
    let mut file = info.as_object().unwrap().clone();
    file.insert("url".into(), json!("mxc://synthetic.test/secret"));
    let mut tampered = encrypted.clone();
    tampered[40] ^= 1;
    let mut tampered_file = file.clone();
    tampered_file.insert("url".into(), json!("mxc://synthetic.test/tampered"));
    let events = [
        (
            "plain",
            image_event(
                "$plain",
                json!({"msgtype":"m.image","body":"cat.jpg","url":"mxc://synthetic.test/cat"}),
            ),
        ),
        (
            "secret",
            image_event(
                "$secret",
                json!({"msgtype":"m.image","body":"x","file":file}),
            ),
        ),
        (
            "tampered",
            image_event(
                "$tampered",
                json!({"msgtype":"m.image","body":"x","file":tampered_file}),
            ),
        ),
        (
            "huge",
            image_event(
                "$huge",
                json!({"msgtype":"m.image","body":"x","url":"mxc://synthetic.test/huge","info":{"size":21 * 1024 * 1024}}),
            ),
        ),
    ];
    server.runtime.block_on(async {
        for (name, event) in events {
            Mock::given(method("GET"))
                .and(path_regex(format!(
                    r"^/_matrix/client/v3/rooms/[^/]+/event/(%24|\$){name}$"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(event))
                .mount(&server.server)
                .await;
        }
        // Matrix v1.19 requires the content type and, since v1.12, the
        // disposition: `inline` only for a type safe to show.
        for (media, body, kind, disposition) in [
            ("cat", JPEG.to_vec(), "image/jpeg", "inline"),
            (
                "secret",
                encrypted.clone(),
                "application/octet-stream",
                "attachment",
            ),
            (
                "tampered",
                tampered,
                "application/octet-stream",
                "attachment",
            ),
            ("huge", JPEG.to_vec(), "image/jpeg", "inline"),
        ] {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/_matrix/client/v1/media/download/synthetic.test/{media}"
                )))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(body, kind)
                        .insert_header(
                            "Content-Disposition",
                            format!("{disposition}; filename=\"{media}\""),
                        ),
                )
                .mount(&server.server)
                .await;
        }
    });
    let dir = TempDir::new().unwrap();
    let core = core_with_worker(dir.path());
    sign_in(&core, &server.uri());
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    wait_for("the room", || {
        matches!(
            call(
                &core,
                Call::Subscribe {
                    room_id: ROOM.into()
                }
            ),
            Outcome::Ok(_)
        )
    });
    let load = |event: &str| {
        call(
            &core,
            Call::LoadImage {
                room_id: ROOM.into(),
                event_id: event.into(),
            },
        )
    };
    let plain: ImageView = value(load("$plain"));
    assert_eq!((plain.width, plain.height, plain.bands), (16, 8, 1));
    let secret: ImageView = value(load("$secret"));
    assert_eq!((secret.width, secret.height), (16, 8));
    assert_ne!(plain.handle, secret.handle);
    assert_eq!(load("$tampered"), Outcome::Error(ErrorCode::Rejected));
    assert_eq!(load("$huge"), Outcome::Error(ErrorCode::Rejected));
    assert_eq!(
        server.requests_to("/_matrix/client/v1/media/download/synthetic.test/huge"),
        0,
        "a declared size over the cap is refused before downloading"
    );
    assert_eq!(load("$missing"), Outcome::Error(ErrorCode::NotFound));
    let requests = server
        .runtime
        .block_on(server.server.received_requests())
        .unwrap_or_default();
    let download = requests
        .iter()
        .find(|request| {
            request
                .url
                .path()
                .ends_with("/media/download/synthetic.test/cat")
        })
        .expect("the plain image was downloaded");
    assert_eq!(
        download
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {TOKEN}").as_str())
    );

    assert_eq!(
        call(
            &core,
            Call::ReleaseImage {
                handle: plain.handle.clone()
            }
        ),
        Outcome::Ok(Value::Null)
    );
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
    assert_eq!(
        call(
            &core,
            Call::ReleaseImage {
                handle: secret.handle
            }
        ),
        Outcome::Error(ErrorCode::NotFound),
        "a lock drops every handle"
    );
    assert_eq!(load("$plain"), Outcome::Error(ErrorCode::Locked));
}

/// Picks one fixed file instead of showing the dialog.
#[cfg(windows)]
struct PickFile(std::path::PathBuf);

#[cfg(windows)]
impl persephone_core::pick::ImagePicker for PickFile {
    fn pick(&self) -> Result<Option<std::path::PathBuf>, ErrorCode> {
        Ok(Some(self.0.clone()))
    }
}

/// The fixture JPEG with a comment that must not be sent, and orientation 6.
#[cfg(windows)]
fn jpeg_with_comment() -> Vec<u8> {
    let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend_from_slice(&6u16.to_le_bytes());
    exif.extend_from_slice(&[0; 6]);
    let mut out = JPEG[..2].to_vec();
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&[0xff, 0xfe, 0, 8]);
    out.extend_from_slice(b"secret");
    out.extend_from_slice(&JPEG[2..]);
    out
}

#[cfg(windows)]
impl Homeserver {
    /// The upload endpoint, and the media configuration the SDK reads
    /// before an upload (it refuses a file over the server's limit).
    fn upload_endpoint(&self) {
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v1/media/config"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"m.upload.size": 52428800})),
                )
                .mount(&self.server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/media/v3/upload"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"content_uri": "mxc://synthetic.test/uploaded"})),
                )
                .mount(&self.server)
                .await;
        });
    }

    fn upload_bodies(&self) -> Vec<Vec<u8>> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST"
                    && request.url.path().contains("media")
                    && request.url.path().ends_with("/upload")
            })
            .map(|request| request.body.clone())
            .collect()
    }
}

#[cfg(windows)]
fn image_core(server: &Homeserver, dir: &Path, picked: &Path) -> Core {
    let core = core_with_worker(dir).with_picker(Arc::new(PickFile(picked.to_path_buf())));
    sign_in(&core, &server.uri());
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    core
}

#[cfg(windows)]
fn pick_draft(core: &Core, room_id: &str) -> persephone_contracts::ImageView {
    match value(call(
        core,
        Call::PickImage {
            room_id: room_id.into(),
        },
    )) {
        persephone_contracts::ImagePick::Draft { preview, .. } => preview,
        other => panic!("{other:?}"),
    }
}

/// GIVEN a picked JPEG with a comment, in a room that is not encrypted WHEN
/// it is sent THEN nothing is uploaded before consent; after consent and a
/// retry the stripped copy is uploaded (no comment) and the event carries
/// the shown size, the MIME type and the generic name only; a second image
/// waits while the first is unsent; the picked file is unchanged.
#[cfg(windows)]
#[test]
fn given_a_picked_image_then_send_image_uploads_only_the_stripped_copy() {
    let server = Homeserver::signed_in();
    server.sending_endpoints(200);
    server.upload_endpoint();
    let dir = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let picked = files.path().join("holiday photo.jpg");
    let original = jpeg_with_comment();
    fs::write(&picked, &original).unwrap();
    let core = image_core(&server, dir.path(), &picked);
    wait_for("the plain room", || {
        value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .iter()
            .any(|room| room.room_id == PLAIN)
    });
    let preview = pick_draft(&core, PLAIN);
    assert_eq!((preview.width, preview.height), (8, 16), "rotated");
    assert_eq!(
        call(
            &core,
            Call::SendImage {
                room_id: ROOM.into(),
                handle: preview.handle.clone()
            }
        ),
        Outcome::Error(ErrorCode::NotFound),
        "a draft is sent only in the room it was picked for"
    );
    let queued: PendingMessage = value(call(
        &core,
        Call::SendImage {
            room_id: PLAIN.into(),
            handle: preview.handle.clone(),
        },
    ));
    assert_eq!(queued.kind, persephone_contracts::PendingKind::Image);
    wait_for("the confirmation request", || {
        pending(&core, PLAIN)[0].state == PendingState::Failed
    });
    assert_eq!(
        pending(&core, PLAIN)[0].failure,
        Some(SendFailure::ConfirmationRequired)
    );
    assert!(
        server.upload_bodies().is_empty(),
        "nothing uploaded before consent"
    );

    // A second image waits while the first is unsent; its draft stays.
    let second = pick_draft(&core, PLAIN);
    assert_eq!(
        call(
            &core,
            Call::SendImage {
                room_id: PLAIN.into(),
                handle: second.handle.clone()
            }
        ),
        Outcome::Error(ErrorCode::LimitExceeded)
    );
    assert_eq!(
        call(
            &core,
            Call::DiscardImage {
                handle: second.handle
            }
        ),
        Outcome::Ok(Value::Null)
    );

    assert_eq!(
        call(
            &core,
            Call::AllowUnencrypted {
                room_id: PLAIN.into()
            }
        ),
        Outcome::Ok(Value::Null)
    );
    assert_eq!(
        call(
            &core,
            Call::RetryMessage {
                local_id: queued.local_id
            }
        ),
        Outcome::Ok(Value::Null)
    );
    wait_for("the send", || {
        pending(&core, PLAIN)[0].state == PendingState::Sent
    });
    let uploads = server.upload_bodies();
    assert_eq!(uploads.len(), 1);
    assert!(uploads[0].starts_with(&[0xff, 0xd8]), "a JPEG");
    assert!(uploads[0].len() < original.len());
    assert!(!uploads[0].windows(6).any(|w| w == b"secret"), "stripped");
    let sends = server.sends();
    assert_eq!(sends.len(), 1);
    assert!(sends[0].contains("/send/m.room.message/"), "{}", sends[0]);
    let event = server.bodies_to(&sends[0]).remove(0);
    assert_eq!(event["msgtype"], "m.image");
    assert_eq!(event["body"], "image.jpg");
    assert_eq!(event["url"], "mxc://synthetic.test/uploaded");
    assert_eq!(event["info"]["w"], 8);
    assert_eq!(event["info"]["h"], 16);
    assert_eq!(event["info"]["size"], uploads[0].len());
    assert_eq!(event["info"]["mimetype"], "image/jpeg");
    assert!(!event.to_string().contains("holiday"), "{event}");
    assert!(event.get("thumbnail_url").is_none());
    assert!(event["info"].get("thumbnail_info").is_none());
    assert_eq!(
        fs::read(&picked).unwrap(),
        original,
        "the file is unchanged"
    );
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
}

/// GIVEN an encrypted room and a device that is not verified WHEN an image
/// is sent THEN it fails as `device_not_verified` with no event sent, and
/// anything the SDK uploaded first is ciphertext, never the image itself.
#[cfg(windows)]
#[test]
fn given_an_encrypted_room_then_an_image_is_never_uploaded_in_plain() {
    let server = Homeserver::signed_in();
    server.sending_endpoints(200);
    server.upload_endpoint();
    let dir = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let picked = files.path().join("photo.jpg");
    fs::write(&picked, jpeg_with_comment()).unwrap();
    let core = image_core(&server, dir.path(), &picked);
    wait_for("the encrypted room", || {
        value::<RoomList>(call(&core, Call::RoomList {}))
            .rooms
            .iter()
            .any(|room| room.room_id == ROOM)
    });
    let preview = pick_draft(&core, ROOM);
    let queued: PendingMessage = value(call(
        &core,
        Call::SendImage {
            room_id: ROOM.into(),
            handle: preview.handle,
        },
    ));
    assert_eq!(queued.kind, persephone_contracts::PendingKind::Image);
    wait_for("the send to end", || {
        pending(&core, ROOM)[0].state == PendingState::Failed
    });
    assert_eq!(
        pending(&core, ROOM)[0].failure,
        Some(SendFailure::DeviceNotVerified)
    );
    assert!(server.sends().is_empty(), "no event: {:?}", server.sends());
    for upload in server.upload_bodies() {
        assert!(
            !upload.starts_with(&[0xff, 0xd8]),
            "the upload is not a JPEG"
        );
        assert!(
            !upload.windows(8).any(|w| w == &JPEG[2..10]),
            "no plain JPEG bytes"
        );
    }
    let state: StoreState = value(call(&core, Call::LockSession {}));
    assert_eq!(state, StoreState::RestartRequired);
    assert!(
        pending(&core, ROOM).is_empty(),
        "the queued image is dropped"
    );
}
