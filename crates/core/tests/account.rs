//! Sign-in, restore, sync and Matrix views against a mock homeserver. The mock
//! speaks plain HTTP on loopback, which only the `dev-disposable-servers`
//! feature accepts; `tools/dev.py check` runs this file with it.
#![cfg(feature = "dev-disposable-servers")]

use persephone_contracts::{
    AccountState, AccountStatus, NoticeKind, RoomList, SignInError, StoreState, TimelineItem,
    TimelinePage,
};
use persephone_core::{
    Core,
    credentials::{CredentialSource, Credentials, PromptError},
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
    matchers::{method, path, path_regex},
};
use zeroize::Zeroizing;

mod common;
use common::Tagged;

const ROOM: &str = "!room:synthetic.test";
const USER: &str = "@alice:synthetic.test";
const TOKEN: &str = "syt_synthetic_access_token_7Q2";
const PASSWORD: &str = "synthetic-password-K8";

/// Answers the prompt with fixed values and counts how often it was asked.
struct Fixed {
    answer: Result<(), PromptError>,
    asked: AtomicUsize,
}

impl Fixed {
    fn new(answer: Result<(), PromptError>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: AtomicUsize::new(0),
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
