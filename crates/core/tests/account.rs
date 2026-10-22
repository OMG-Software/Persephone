//! Sign-in, restore, sync and Matrix views against a mock homeserver. The mock
//! speaks plain HTTP on loopback, which only the `dev-disposable-servers`
//! feature accepts; `tools/dev.py check` runs this file with it.
#![cfg(feature = "dev-disposable-servers")]

use persephone_contracts::{
    AccountState, AccountStatus, IdentityState, NoticeKind, RoomList, SecurityError,
    SecurityOperation, SecurityStatus, SignInError, StoreState, TimelineItem, TimelinePage,
};
use persephone_core::{
    Core,
    credentials::{CredentialSource, Credentials, PromptError, SecretPrompt},
    session::{AdmissionError, Session},
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
use common::Tagged;

const ROOM: &str = "!room:synthetic.test";
const USER: &str = "@alice:synthetic.test";
const TOKEN: &str = "syt_synthetic_access_token_7Q2";
const PASSWORD: &str = "synthetic-password-K8";

/// Answers the prompts with fixed values, counts how often each was asked and
/// keeps the recovery key it is shown.
struct Fixed {
    answer: Result<(), PromptError>,
    asked: AtomicUsize,
    /// Answers in order; the last one repeats.
    secrets: std::sync::Mutex<Vec<Result<String, PromptError>>>,
    secrets_asked: AtomicUsize,
    shown: std::sync::Mutex<Option<String>>,
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
        *self.shown.lock().unwrap() = Some(key.to_owned());
        Ok(())
    }
}

fn room_state() -> Value {
    json!([
        {"type":"m.room.create","state_key":"","sender":USER,"event_id":"$create","origin_server_ts":1,"content":{"creator":USER,"room_version":"11"}},
        {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$join","origin_server_ts":2,"content":{"membership":"join"}},
        {"type":"m.room.name","state_key":"","sender":USER,"event_id":"$name","origin_server_ts":3,"content":{"name":"Synthetic <b>room</b>"}},
        {"type":"m.room.encryption","state_key":"","sender":USER,"event_id":"$encryption","origin_server_ts":4,"content":{"algorithm":"m.megolm.v1.aes-sha2"}}
    ])
}

fn text(id: &str, body: &str) -> Value {
    json!({"type":"m.room.message","sender":USER,"event_id":id,"origin_server_ts":10,"content":{"msgtype":"m.text","body":body}})
}

/// History, newest first as `/messages` backward returns it.
fn history() -> Value {
    json!([
        text("$text", "Hello from <script>alert(1)</script>"),
        {"type":"m.room.message","sender":USER,"event_id":"$image","origin_server_ts":9,"content":{"msgtype":"m.image","body":"cat.png","url":"mxc://synthetic.test/cat"}},
        {"type":"m.room.encrypted","sender":USER,"event_id":"$secret","origin_server_ts":8,"content":{"algorithm":"m.megolm.v1.aes-sha2","ciphertext":"AwgAEnACgAkLmt6qF84IK","device_id":"OTHER","sender_key":"c2VuZGVyLWtleQ","session_id":"c2Vzc2lvbi1pZA"}},
        {"type":"m.reaction","sender":USER,"event_id":"$reaction","origin_server_ts":7,"content":{"m.relates_to":{"rel_type":"m.annotation","event_id":"$text","key":"+1"}}}
    ])
}

/// Classic sync like a live room: each response has a new batch token and a
/// new message.
#[derive(Default)]
struct LiveRoom {
    round: AtomicUsize,
}

impl wiremock::Respond for LiveRoom {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        let n = self.round.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(json!({
            "next_batch": format!("classic-{n}"),
            "rooms":{"join":{ROOM:{"state":{"events":room_state()},"timeline":{"events":[text(&format!("$live{n}"), "live")],"limited":true,"prev_batch":"history"},"ephemeral":{"events":[]},"account_data":{"events":[]}}}},
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
            let versions = if sliding {
                json!({"versions":["v1.15"],"unstable_features":{"org.matrix.simplified_msc3575":true}})
            } else {
                json!({"versions":["v1.11"]})
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
                .respond_with(LiveRoom::default())
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "pos":"sliding-pos","lists":{"rooms":{"count":1,"ops":[{"op":"SYNC","range":[0,0],"room_ids":[ROOM]}]}},
                    "rooms":{ROOM:{"initial":true,"joined_count":1,"invited_count":0,"required_state":room_state(),"timeline":[text("$live","live")],"num_live":1,"prev_batch":"history","notification_count":0,"highlight_count":0}},
                    "extensions":{"e2ee":{"device_lists":{"changed":[],"left":[]},"device_one_time_keys_count":{"signed_curve25519":50}},"to_device":{"events":[],"next_batch":"to-device-pos"}}
                })))
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
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/device_signing/upload"))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                    "flows": [{"stages": ["m.login.password"]}],
                    "params": {},
                    "session": "uia-session-1"
                })))
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
}

fn make_core(dir: &Path, source: Arc<dyn CredentialSource>) -> Core {
    Core::default()
        .with_credentials(source)
        .with_session(Session::open(dir.to_path_buf(), Box::new(Tagged(1))))
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
        // Oldest first; the reaction is not shown; nothing is hidden silently.
        let shown: Vec<String> = page
            .items
            .iter()
            .map(|item| match item {
                TimelineItem::Message { event_id, body, .. } => format!("{event_id}={body}"),
                TimelineItem::Notice {
                    event_id, notice, ..
                } => format!("{event_id}:{notice:?}"),
                TimelineItem::Gap { .. } => "gap".into(),
            })
            .collect();
        assert_eq!(
            shown,
            [
                format!("$secret:{:?}", NoticeKind::Undecryptable),
                format!("$image:{:?}", NoticeKind::Unsupported),
                "$text=Hello from <script>alert(1)</script>".to_string(),
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
    assert_eq!(state, StoreState::Protected);
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
    assert_eq!(
        state,
        StoreState::Protected,
        "unlocked before the prompt returns"
    );
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
        assert!(!dir.path().join("store-key.v1").exists(), "key deleted");
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
        !dir.path().join("store-key.v1").exists(),
        "key deleted anyway"
    );
}

#[test]
fn given_no_account_then_sign_out_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let core = make_core(dir.path(), Fixed::new(Ok(())));
    let state: StoreState = value(call(&core, Call::SignOut {}));
    assert_eq!(state, StoreState::Protected);
    assert!(dir.path().join("store-key.v1").exists());
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
    assert!(dir.path().join("store-key.v1").exists(), "nothing deleted");
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
    assert!(!dir.path().join("store-key.v1").exists());
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
            dir.path().join("store-key.v1").exists(),
            "key deleted only after the answer"
        );
        assert_eq!(signing_out.join().unwrap(), Ok(StoreState::RestartRequired));
    });
    assert!(!dir.path().join("store-key.v1").exists());
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
        .with_session(Session::open(dir.path().into(), Box::new(Tagged(1))));
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
