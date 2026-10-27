//! Changing the password, email addresses and phone numbers, the recovery
//! key, the notification level and desktop notifications, against a mock
//! homeserver
//! that answers as Matrix v1.19 does: `versions` lists `v1.19`, a failed
//! authentication stage carries its errcode, and an unknown endpoint answers
//! `M_UNRECOGNIZED`. The mock speaks plain HTTP on loopback, which only the
//! `dev-disposable-servers` feature accepts; `tools/dev.py check` runs this
//! file with it.
#![cfg(feature = "dev-disposable-servers")]

use persephone_contracts::{
    AccountChange, AccountState, IdentityState, NotificationLevel, NotificationSettings,
    RecoveryState, SecurityError, SecurityOperation, SecurityStatus, ThreePidList, ThreePidMedium,
};
use persephone_core::{
    Core,
    credentials::{CredentialSource, Credentials, PromptError, SecretPrompt},
};
use persephone_core_protocol::{Call, ErrorCode, Outcome, Request};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{any, body_partial_json, method, path, path_regex},
};
use zeroize::Zeroizing;

mod common;

const ROOM: &str = "!room:synthetic.test";
const USER: &str = "@alice:synthetic.test";
const BOB: &str = "@bob:synthetic.test";
const TOKEN: &str = "syt_synthetic_access_token_9Z4";
const PASSWORD: &str = "synthetic-password-K8";
const EMAIL: &str = "alice@example.org";
/// The text of every message the mock sends; it must never reach a toast.
const SECRET_TEXT: &str = "the launch code is 0000";

/// Answers the sign-in prompt with the account, then each secret prompt from
/// a script, and records what every secret prompt asked for. Keeps every
/// recovery key it is shown, answers the recovery-key change warning with
/// `change`, and offers `save_to` in the Save dialog.
struct Answers {
    secrets: Mutex<VecDeque<Result<String, PromptError>>>,
    seen: Mutex<Vec<String>>,
    shown: Mutex<Vec<String>>,
    change: Mutex<Result<bool, PromptError>>,
    changes_asked: AtomicUsize,
    save_to: Mutex<Option<PathBuf>>,
}

impl Answers {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            secrets: Mutex::default(),
            seen: Mutex::default(),
            shown: Mutex::default(),
            change: Mutex::new(Err(PromptError::Cancelled)),
            changes_asked: AtomicUsize::new(0),
            save_to: Mutex::default(),
        })
    }

    /// Queues the next answers.
    fn answer(&self, answers: &[Result<&str, PromptError>]) {
        let mut secrets = self.secrets.lock().unwrap();
        secrets.clear();
        secrets.extend(answers.iter().map(|answer| answer.map(str::to_owned)));
        self.seen.lock().unwrap().clear();
    }

    /// The prompts asked since the last `answer`, by kind.
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl CredentialSource for Answers {
    fn ask(&self, _host: &str) -> Result<Credentials, PromptError> {
        Ok(Credentials {
            username: Zeroizing::new("alice".into()),
            password: Zeroizing::new(PASSWORD.into()),
        })
    }

    fn ask_secret(&self, prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        let kind = match prompt {
            SecretPrompt::CurrentPassword { user_id, .. } => {
                assert_eq!(user_id, USER, "the prompt names the signed-in account");
                "current"
            }
            SecretPrompt::ChangedPassword { step, .. } => match step {
                persephone_core::credentials::NewPassword::Choose => "new",
                persephone_core::credentials::NewPassword::Confirm => "confirm",
                persephone_core::credentials::NewPassword::Mismatch => "mismatch",
            },
            SecretPrompt::PhoneCode { .. } => "code",
            _ => "other",
        };
        self.seen.lock().unwrap().push(kind.to_owned());
        self.secrets
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(PromptError::Cancelled))
            .map(Zeroizing::new)
    }

    fn show_recovery_key(&self, user_id: &str, key: &str) -> Result<(), PromptError> {
        assert_eq!(user_id, USER);
        self.shown.lock().unwrap().push(key.to_owned());
        Ok(())
    }

    fn confirm_recovery_key_change(&self) -> Result<bool, PromptError> {
        self.changes_asked.fetch_add(1, Ordering::SeqCst);
        *self.change.lock().unwrap()
    }

    fn choose_recovery_key_file(&self) -> Result<Option<PathBuf>, PromptError> {
        Ok(self.save_to.lock().unwrap().clone())
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn room_state() -> Value {
    json!([
        {"type":"m.room.create","state_key":"","sender":USER,"event_id":"$create","origin_server_ts":1,"content":{"room_version":"11"}},
        {"type":"m.room.member","state_key":USER,"sender":USER,"event_id":"$join","origin_server_ts":2,"content":{"membership":"join"}},
        {"type":"m.room.member","state_key":BOB,"sender":BOB,"event_id":"$bob","origin_server_ts":3,"content":{"membership":"join"}},
        {"type":"m.room.member","state_key":"@carol:synthetic.test","sender":"@carol:synthetic.test","event_id":"$carol","origin_server_ts":3,"content":{"membership":"join"}},
        {"type":"m.room.name","state_key":"","sender":USER,"event_id":"$name","origin_server_ts":4,"content":{"name":"Plans"}}
    ])
}

/// Classic sync: each round brings a new message from Bob, sent now, and the
/// push rules when the test gives them.
struct LiveRoom {
    round: AtomicUsize,
    push_rules: Option<Value>,
}

impl wiremock::Respond for LiveRoom {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        let n = self.round.fetch_add(1, Ordering::SeqCst);
        let message = json!({"type":"m.room.message","sender":BOB,"event_id":format!("$live{n}"),
            "origin_server_ts":now_millis(),"content":{"msgtype":"m.text","body":SECRET_TEXT}});
        let account_data = match &self.push_rules {
            Some(rules) => json!([{"type":"m.push_rules","content":rules}]),
            None => json!([]),
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "next_batch": format!("classic-{n}"),
            "account_data":{"events":account_data},
            "rooms":{"join":{ROOM:{
                "summary":{"m.joined_member_count":3,"m.invited_member_count":0},
                "state":{"events":room_state()},
                "timeline":{"events":[message],"limited":false},
                "ephemeral":{"events":[]},"account_data":{"events":[]},
                "unread_notifications":{"notification_count":1,"highlight_count":0}
            }}},
            "to_device":{"events":[]},"device_lists":{"changed":[],"left":[]},
            "device_one_time_keys_count":{"signed_curve25519":50}
        }))
    }
}

/// An authentication round as Matrix v1.19 answers it: the password flow,
/// and on a failed stage its errcode as well.
fn uia(session: &str, failed: bool) -> ResponseTemplate {
    let mut body = json!({"flows":[{"stages":["m.login.password"]}],"params":{},"session":session});
    if failed {
        body["errcode"] = json!("M_FORBIDDEN");
        body["error"] = json!("Invalid password");
        body["completed"] = json!([]);
    }
    ResponseTemplate::new(401).set_body_json(body)
}

struct Homeserver {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
}

impl Homeserver {
    fn start(push_rules: Option<Value>) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/versions"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"versions":["v1.19"]})),
                )
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/login"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}),
                ))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/logout"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .mount(&server)
                .await;
            for (p, body) in [
                (
                    "/_matrix/client/v3/keys/upload",
                    json!({"one_time_key_counts":{"signed_curve25519":50}}),
                ),
                (
                    "/_matrix/client/v3/keys/query",
                    json!({"device_keys":{},"failures":{}}),
                ),
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
                    push_rules,
                })
                // Long-poll a little, so the rounds do not race.
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/capabilities"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"capabilities":{"m.3pid_changes":{"enabled":true}}})),
                )
                .mount(&server)
                .await;
            // Anything not mounted is an endpoint this homeserver lacks.
            Mock::given(path_regex("^/_matrix/"))
                .respond_with(ResponseTemplate::new(404).set_body_json(
                    json!({"errcode":"M_UNRECOGNIZED","error":"Unrecognized request"}),
                ))
                .with_priority(10)
                .mount(&server)
                .await;
            server
        });
        Self { runtime, server }
    }

    fn mount(&self, mock: Mock) {
        self.runtime.block_on(mock.mount(&self.server));
    }

    /// `endpoint` answers an authentication round, then accepts the request
    /// only with `PASSWORD` and the round's session; another password fails
    /// the stage.
    fn with_password_auth(&self, http: &str, endpoint: &str, session: &str, done: Value) {
        self.mount(
            Mock::given(method(http))
                .and(path(endpoint))
                .and(body_partial_json(json!({"auth":{
                    "type":"m.login.password","password":PASSWORD,"session":session,
                    "identifier":{"type":"m.id.user","user":USER}
                }})))
                .respond_with(ResponseTemplate::new(200).set_body_json(done))
                .with_priority(1),
        );
        self.mount(
            Mock::given(method(http))
                .and(path(endpoint))
                .and(body_partial_json(
                    json!({"auth":{"type":"m.login.password"}}),
                ))
                .respond_with(uia(session, true))
                .with_priority(2),
        );
        self.mount(
            Mock::given(method(http))
                .and(path(endpoint))
                .respond_with(uia(session, false))
                .with_priority(3),
        );
    }

    fn uri(&self) -> String {
        self.server.uri()
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

fn change(core: &Core, request: Call) -> AccountChange {
    value(call(core, request))
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn signed_in(server: &Homeserver, dir: &Path, source: Arc<Answers>) -> Core {
    let core = Core::default()
        .with_credentials(source)
        .with_session(common::open_session(dir));
    let _ = call(
        &core,
        Call::SignIn {
            homeserver: server.uri(),
        },
    );
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    core
}

const PASSWORD_PATH: &str = "/_matrix/client/v3/account/password";

/// GIVEN the current password and a new one typed twice WHEN the page asks
/// to change the password and keep the other sessions THEN the core asks in
/// that order, sends the change once without and once with the password and
/// the round's session, says not to sign out other devices, and reports done.
#[test]
fn given_the_passwords_then_the_password_changes_and_other_sessions_stay() {
    let server = Homeserver::start(None);
    server.with_password_auth("POST", PASSWORD_PATH, "uia-password", json!({}));
    let dir = TempDir::new().unwrap();
    let answers = Answers::new();
    let core = signed_in(&server, dir.path(), answers.clone());
    answers.answer(&[Ok(PASSWORD), Ok("new-password-1"), Ok("new-password-1")]);
    assert_eq!(
        change(
            &core,
            Call::ChangePassword {
                sign_out_others: false
            }
        ),
        AccountChange::Done
    );
    assert_eq!(answers.seen(), ["current", "new", "confirm"]);
    let bodies = server.bodies_to(PASSWORD_PATH);
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[0].get("auth").is_none(),
        "the first asks how to authenticate"
    );
    assert_eq!(bodies[1]["new_password"], "new-password-1");
    assert_eq!(bodies[1]["logout_devices"], false);
    assert_eq!(bodies[1]["auth"]["session"], "uia-password");
}

/// GIVEN two different new passwords, then the same one twice WHEN the
/// password is changed with "sign out my other sessions" THEN the core asks
/// again after the mismatch, and the change keeps the specification's
/// default of signing the other devices out.
#[test]
fn given_a_mismatch_then_the_core_asks_again_and_signs_out_others() {
    let server = Homeserver::start(None);
    server.with_password_auth("POST", PASSWORD_PATH, "uia-password", json!({}));
    let dir = TempDir::new().unwrap();
    let answers = Answers::new();
    let core = signed_in(&server, dir.path(), answers.clone());
    answers.answer(&[
        Ok(PASSWORD),
        Ok("one"),
        Ok("two"),
        Ok("three-3"),
        Ok("three-3"),
    ]);
    assert_eq!(
        change(
            &core,
            Call::ChangePassword {
                sign_out_others: true
            }
        ),
        AccountChange::Done
    );
    assert_eq!(
        answers.seen(),
        ["current", "new", "confirm", "mismatch", "confirm"]
    );
    let bodies = server.bodies_to(PASSWORD_PATH);
    assert_eq!(bodies[1]["new_password"], "three-3");
    // `true` is the default, so it may be left out; never `false`.
    assert_ne!(bodies[1]["logout_devices"], false);
}

/// GIVEN a wrong current password WHEN the password is changed THEN the
/// failed stage (with its errcode) is `invalid_password`, after one try with
/// the password; GIVEN a closed window THEN nothing is sent at all.
#[test]
fn given_a_wrong_password_or_a_closed_window_then_the_change_stops() {
    let server = Homeserver::start(None);
    server.with_password_auth("POST", PASSWORD_PATH, "uia-password", json!({}));
    let dir = TempDir::new().unwrap();
    let answers = Answers::new();
    let core = signed_in(&server, dir.path(), answers.clone());
    answers.answer(&[Ok("not-the-password"), Ok("new-pw"), Ok("new-pw")]);
    assert_eq!(
        change(
            &core,
            Call::ChangePassword {
                sign_out_others: false
            }
        ),
        AccountChange::InvalidPassword
    );
    assert_eq!(server.bodies_to(PASSWORD_PATH).len(), 2, "no retry");
    for (answer, expected) in [
        (PromptError::Cancelled, AccountChange::Cancelled),
        (PromptError::Unsupported, AccountChange::Unsupported),
    ] {
        answers.answer(&[Ok(PASSWORD), Err(answer)]);
        assert_eq!(
            change(
                &core,
                Call::ChangePassword {
                    sign_out_others: false
                }
            ),
            expected
        );
    }
    assert_eq!(
        server.bodies_to(PASSWORD_PATH).len(),
        2,
        "nothing more was sent"
    );
}

const EMAIL_TOKEN: &str = "/_matrix/client/v3/account/3pid/email/requestToken";
const PHONE_TOKEN: &str = "/_matrix/client/v3/account/3pid/msisdn/requestToken";
const ADD: &str = "/_matrix/client/v3/account/3pid/add";
const SUBMIT: &str = "/_matrix/client/unstable/add_threepid/msisdn/submit_token";

/// GIVEN an email address WHEN the page asks to add it THEN the homeserver
/// is asked to send a link (to no identity server), the address shows as
/// waiting, and adding it asks for the password and sends the same session
/// and secret; once added, nothing waits.
#[test]
fn given_an_email_address_then_it_is_confirmed_and_added_with_the_password() {
    let server = Homeserver::start(None);
    server.mount(
        Mock::given(method("POST"))
            .and(path(EMAIL_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sid":"sid-email"}))),
    );
    server.mount(
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/account/3pid"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"threepids":[
                    {"medium":"email","address":"old@example.org","validated_at":1,"added_at":1}
                ]})),
            ),
    );
    server.with_password_auth("POST", ADD, "uia-add", json!({}));
    let dir = TempDir::new().unwrap();
    let answers = Answers::new();
    let core = signed_in(&server, dir.path(), answers.clone());

    assert_eq!(
        call(
            &core,
            Call::RequestEmailToken {
                email: "not an address".into()
            }
        ),
        Outcome::Error(ErrorCode::InvalidArgument)
    );
    assert!(server.bodies_to(EMAIL_TOKEN).is_empty(), "nothing was sent");
    assert_eq!(
        change(
            &core,
            Call::RequestEmailToken {
                email: EMAIL.into()
            }
        ),
        AccountChange::TokenSent
    );
    let requested = &server.bodies_to(EMAIL_TOKEN)[0];
    assert_eq!(requested["email"], EMAIL);
    assert!(requested.get("id_server").is_none() && requested.get("id_access_token").is_none());

    let listed: ThreePidList = value(call(&core, Call::ThreePids {}));
    assert_eq!(listed.three_pids[0].address, "old@example.org");
    assert!(listed.can_change);
    let pending = listed.pending.expect("the address waits");
    assert_eq!(
        (pending.medium, pending.address.as_str()),
        (ThreePidMedium::Email, EMAIL)
    );

    answers.answer(&[Ok(PASSWORD)]);
    assert_eq!(change(&core, Call::AddThreePid {}), AccountChange::Done);
    assert_eq!(answers.seen(), ["current"]);
    let added = server.bodies_to(ADD);
    assert_eq!(added[1]["sid"], "sid-email");
    assert_eq!(added[1]["client_secret"], requested["client_secret"]);
    let listed: ThreePidList = value(call(&core, Call::ThreePids {}));
    assert!(listed.pending.is_none());
    answers.answer(&[]);
    assert_eq!(
        change(&core, Call::AddThreePid {}),
        AccountChange::NothingPending
    );
    assert!(answers.seen().is_empty(), "no window without an address");
}

/// GIVEN a link not opened yet WHEN the address is added THEN the
/// homeserver's `M_THREEPID_AUTH_FAILED` is `not_confirmed` and the address
/// keeps waiting, so the user can try again.
#[test]
fn given_an_unopened_link_then_the_address_keeps_waiting() {
    let server = Homeserver::start(None);
    server.mount(
        Mock::given(method("POST"))
            .and(path(EMAIL_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sid":"sid-email"}))),
    );
    server.mount(Mock::given(method("POST")).and(path(ADD)).respond_with(
        ResponseTemplate::new(400).set_body_json(
            json!({"errcode":"M_THREEPID_AUTH_FAILED","error":"No validated 3pid session found"}),
        ),
    ));
    server.mount(
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/account/3pid"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"threepids":[]}))),
    );
    let dir = TempDir::new().unwrap();
    let answers = Answers::new();
    let core = signed_in(&server, dir.path(), answers.clone());
    assert_eq!(
        change(
            &core,
            Call::RequestEmailToken {
                email: EMAIL.into()
            }
        ),
        AccountChange::TokenSent
    );
    answers.answer(&[Ok(PASSWORD)]);
    assert_eq!(
        change(&core, Call::AddThreePid {}),
        AccountChange::NotConfirmed
    );
    let listed: ThreePidList = value(call(&core, Call::ThreePids {}));
    assert!(listed.pending.is_some());
}

/// GIVEN homeservers that cannot add a phone number: one that does not
/// support the medium, one without the endpoint, one that needs an identity
/// server to check the code, and one whose capabilities forbid changes WHEN
/// a number is asked for THEN each gives a fixed code, and the last sends no
/// token request at all.
#[test]
fn given_a_homeserver_without_phone_support_then_each_case_has_its_code() {
    let dir = TempDir::new().unwrap();
    let answers = Answers::new();
    let phone = || Call::RequestPhoneToken {
        country: "GB".into(),
        phone_number: "07700900123".into(),
    };
    {
        let server = Homeserver::start(None);
        server.mount(
            Mock::given(method("POST"))
                .and(path(PHONE_TOKEN))
                .respond_with(ResponseTemplate::new(400).set_body_json(
                    json!({"errcode":"M_THREEPID_MEDIUM_NOT_SUPPORTED","error":"no"}),
                )),
        );
        let core = signed_in(&server, dir.path(), answers.clone());
        assert_eq!(change(&core, phone()), AccountChange::MediumNotSupported);
        // No endpoint at all: the mock's catch-all `M_UNRECOGNIZED`.
        assert_eq!(
            change(
                &core,
                Call::RequestEmailToken {
                    email: EMAIL.into()
                }
            ),
            AccountChange::MediumNotSupported
        );
        let _ = call(&core, Call::SignOut {});
    }
    let dir = TempDir::new().unwrap();
    {
        let server = Homeserver::start(None);
        server.mount(
            Mock::given(method("POST"))
                .and(path(PHONE_TOKEN))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sid":"sid-phone"}))),
        );
        let core = signed_in(&server, dir.path(), answers.clone());
        assert_eq!(change(&core, phone()), AccountChange::MediumNotSupported);
        let listed: ThreePidList = value(call(&core, Call::ThreePids {}));
        assert!(listed.pending.is_none(), "nothing waits");
        assert!(listed.three_pids.is_empty(), "M_UNRECOGNIZED lists none");
        let _ = call(&core, Call::SignOut {});
    }
    let dir = TempDir::new().unwrap();
    let server = Homeserver::start(None);
    server.mount(
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/capabilities"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"capabilities":{"m.3pid_changes":{"enabled":false}}})),
            )
            .with_priority(1),
    );
    let core = signed_in(&server, dir.path(), answers.clone());
    assert_eq!(change(&core, phone()), AccountChange::ChangesDisabled);
    assert!(server.bodies_to(PHONE_TOKEN).is_empty());
    let listed: ThreePidList = value(call(&core, Call::ThreePids {}));
    assert!(!listed.can_change);
}

/// GIVEN a homeserver that sends a code and gives its own `submit_url` WHEN
/// the number is added THEN the core asks for the code in its own window,
/// sends it there with the session and secret, then asks for the password
/// and adds the number; a code sent to another host is never followed.
#[test]
fn given_a_phone_number_then_its_code_and_the_password_are_asked_in_the_core() {
    let server = Homeserver::start(None);
    let submit_url = format!("{}{SUBMIT}", server.uri());
    server.mount(
        Mock::given(method("POST"))
            .and(path(PHONE_TOKEN))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"sid":"sid-phone","submit_url":submit_url})),
            ),
    );
    server.mount(
        Mock::given(method("POST"))
            .and(path(SUBMIT))
            .and(body_partial_json(
                json!({"token":"123456","sid":"sid-phone"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success":true}))),
    );
    server.with_password_auth("POST", ADD, "uia-add", json!({}));
    let dir = TempDir::new().unwrap();
    let answers = Answers::new();
    let core = signed_in(&server, dir.path(), answers.clone());
    assert_eq!(
        change(
            &core,
            Call::RequestPhoneToken {
                country: "GB".into(),
                phone_number: "07700900123".into()
            }
        ),
        AccountChange::TokenSent
    );
    let requested = &server.bodies_to(PHONE_TOKEN)[0];
    assert_eq!(requested["country"], "GB");
    // A wrong code is refused, and the number keeps waiting.
    answers.answer(&[Ok("999999")]);
    server.mount(
        Mock::given(method("POST"))
            .and(path(SUBMIT))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"errcode":"M_THREEPID_AUTH_FAILED","error":"wrong"})),
            )
            .with_priority(5),
    );
    assert_eq!(
        change(&core, Call::AddThreePid {}),
        AccountChange::NotConfirmed
    );
    assert_eq!(answers.seen(), ["code"]);
    answers.answer(&[Ok("123456"), Ok(PASSWORD)]);
    assert_eq!(change(&core, Call::AddThreePid {}), AccountChange::Done);
    assert_eq!(answers.seen(), ["code", "current"]);
    let submitted = server.bodies_to(SUBMIT);
    assert_eq!(
        submitted.last().unwrap()["client_secret"],
        requested["client_secret"]
    );
    assert_eq!(server.bodies_to(ADD)[1]["sid"], "sid-phone");
}

/// GIVEN an address on the account WHEN the page removes it THEN the
/// homeserver is asked to delete it without naming an identity server;
/// GIVEN a malformed address THEN nothing is sent.
#[test]
fn given_an_address_then_it_is_removed_without_an_identity_server() {
    let server = Homeserver::start(None);
    let delete = "/_matrix/client/v3/account/3pid/delete";
    server.mount(Mock::given(method("POST")).and(path(delete)).respond_with(
        ResponseTemplate::new(200).set_body_json(json!({"id_server_unbind_result":"no-support"})),
    ));
    let dir = TempDir::new().unwrap();
    let core = signed_in(&server, dir.path(), Answers::new());
    assert_eq!(
        change(
            &core,
            Call::RemoveThreePid {
                medium: ThreePidMedium::Email,
                address: EMAIL.into()
            }
        ),
        AccountChange::Done
    );
    let body = &server.bodies_to(delete)[0];
    assert_eq!(
        (body["medium"].as_str(), body["address"].as_str()),
        (Some("email"), Some(EMAIL))
    );
    assert!(body.get("id_server").is_none());
    assert_eq!(
        call(
            &core,
            Call::RemoveThreePid {
                medium: ThreePidMedium::Msisdn,
                address: "+44 7700".into()
            }
        ),
        Outcome::Error(ErrorCode::InvalidArgument)
    );
    assert_eq!(server.bodies_to(delete).len(), 1);
}

/// The server's default push rules for `USER`, with the master rule on or
/// off.
fn push_rules(master: bool) -> Value {
    let user = matrix_sdk::ruma::UserId::parse(USER).unwrap();
    let mut rules =
        serde_json::to_value(matrix_sdk::ruma::push::Ruleset::server_default(&user)).unwrap();
    for rule in rules["override"].as_array_mut().unwrap() {
        if rule["rule_id"] == ".m.rule.master" {
            rule["enabled"] = json!(master);
        }
    }
    json!({"global": rules})
}

/// GIVEN push rules with the master rule on WHEN the level is read THEN it
/// is `none`; WHEN the page sets a level THEN the homeserver's standard push
/// rules change: the master rule for `none`, the master rule off and the
/// default room rules for `mentions`. `custom` is never set.
#[test]
fn given_push_rules_then_the_level_reads_and_sets_them() {
    let server = Homeserver::start(Some(push_rules(true)));
    server.mount(
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/pushrules/global/[a-z]+/[^/]+/(enabled|actions)$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))),
    );
    let dir = TempDir::new().unwrap();
    let core = signed_in(&server, dir.path(), Answers::new());
    wait_for("the push rules", || {
        matches!(
            call(&core, Call::NotificationLevel {}),
            Outcome::Ok(ref level) if level == &json!({"level":"none"})
        )
    });
    assert_eq!(
        call(
            &core,
            Call::SetNotificationLevel {
                level: NotificationLevel::Custom
            }
        ),
        Outcome::Error(ErrorCode::InvalidArgument)
    );
    assert_eq!(
        call(
            &core,
            Call::SetNotificationLevel {
                level: NotificationLevel::Mentions
            }
        ),
        Outcome::Ok(Value::Null)
    );
    let master = "/_matrix/client/v3/pushrules/global/override/.m.rule.master/enabled";
    assert_eq!(server.bodies_to(master), [json!({"enabled": false})]);
    let actions: Vec<Value> = [
        ".m.rule.message",
        ".m.rule.encrypted",
        ".m.rule.room_one_to_one",
        ".m.rule.encrypted_room_one_to_one",
    ]
    .iter()
    .flat_map(|rule| {
        server.bodies_to(&format!(
            "/_matrix/client/v3/pushrules/global/underride/{rule}/actions"
        ))
    })
    .collect();
    assert_eq!(actions.len(), 4, "each default room rule");
    assert!(actions.iter().all(|body| body == &json!({"actions": []})));
    assert_eq!(
        call(
            &core,
            Call::SetNotificationLevel {
                level: NotificationLevel::None
            }
        ),
        Outcome::Ok(Value::Null)
    );
    assert_eq!(
        server.bodies_to(master).last(),
        Some(&json!({"enabled": true}))
    );
    let _: NotificationSettings = value(call(&core, Call::NotificationLevel {}));
}

/// GIVEN messages from another member that the push rules notify of WHEN
/// desktop notifications are off THEN nothing waits to be shown; WHEN they
/// are turned on THEN a notification waits with the room's and sender's
/// names, never the message text.
#[test]
fn given_desktop_notifications_then_only_names_wait_to_be_shown() {
    let server = Homeserver::start(None);
    let dir = TempDir::new().unwrap();
    let core = signed_in(&server, dir.path(), Answers::new());
    // A few rounds with notifications off.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(core.pending_toast(), None);
    assert_eq!(
        call(
            &core,
            Call::SetAccountSettings {
                enter_sends: true,
                chat_text_percent: 100,
                interface_text_percent: 100,
                desktop_notifications: true,
            }
        ),
        Outcome::Ok(Value::Null)
    );
    wait_for("a desktop notification", || core.pending_toast().is_some());
    let (room, sender) = core.pending_toast().unwrap();
    assert_eq!(room, "Plans");
    assert_eq!(sender, BOB);
    assert!(!room.contains("launch") && !sender.contains("launch"));
}

fn security(core: &Core) -> SecurityStatus {
    value(call(core, Call::SecurityStatus {}))
}

/// What the key homeserver keeps: this device's keys, the cross-signing
/// keys, the account data (secret storage) and the key backup.
#[derive(Default)]
struct KeyState {
    device: Option<Value>,
    master: Option<Value>,
    self_signing: Option<Value>,
    user_signing: Option<Value>,
    account_data: serde_json::Map<String, Value>,
    backup: Option<Value>,
    batch: u64,
}

/// A homeserver that keeps keys and account data, so that setting up and
/// changing recovery see their own uploads again, as on a real server. The
/// cross-signing upload needs `PASSWORD` through an authentication round.
#[derive(Clone, Default)]
struct KeyServer(Arc<Mutex<KeyState>>);

fn sign_into(stored: &mut Value, signed: &Value) {
    for (signer, keys) in signed["signatures"].as_object().into_iter().flatten() {
        let entry = stored["signatures"][signer.as_str()].take();
        let mut merged = entry.as_object().cloned().unwrap_or_default();
        for (key, signature) in keys.as_object().into_iter().flatten() {
            merged.insert(key.clone(), signature.clone());
        }
        stored["signatures"][signer.as_str()] = Value::Object(merged);
    }
}

impl wiremock::Respond for KeyServer {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let path = request.url.path().to_owned();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let http = request.method.as_str();
        let mut state = self.0.lock().unwrap();
        let ok = |reply: Value| ResponseTemplate::new(200).set_body_json(reply);
        let not_found = |errcode: &str| {
            ResponseTemplate::new(404).set_body_json(json!({"errcode":errcode,"error":"none"}))
        };
        if path == "/_matrix/client/versions" {
            ok(json!({"versions":["v1.19"]}))
        } else if path == "/_matrix/client/v3/login" {
            ok(json!({"user_id":USER,"access_token":TOKEN,"device_id":"PERSEPHONE1"}))
        } else if path == "/_matrix/client/v3/keys/upload" {
            if let Some(keys) = body.get("device_keys") {
                state.device = Some(keys.clone());
            }
            ok(json!({"one_time_key_counts":{"signed_curve25519":50}}))
        } else if path == "/_matrix/client/v3/keys/query" {
            let mut reply = json!({"device_keys":{USER:{}},"failures":{}});
            if let Some(device) = &state.device {
                reply["device_keys"][USER]["PERSEPHONE1"] = device.clone();
            }
            for (field, key) in [
                ("master_keys", &state.master),
                ("self_signing_keys", &state.self_signing),
                ("user_signing_keys", &state.user_signing),
            ] {
                if let Some(key) = key {
                    reply[field] = json!({USER: key});
                }
            }
            ok(reply)
        } else if path == "/_matrix/client/v3/keys/device_signing/upload" {
            if body["auth"]["password"] != PASSWORD {
                return uia("uia-signing", body.get("auth").is_some());
            }
            state.master = body.get("master_key").cloned();
            state.self_signing = body.get("self_signing_key").cloned();
            state.user_signing = body.get("user_signing_key").cloned();
            ok(json!({}))
        } else if path == "/_matrix/client/v3/keys/signatures/upload" {
            for (key_id, signed) in body[USER].as_object().into_iter().flatten() {
                if key_id == "PERSEPHONE1" {
                    if let Some(device) = state.device.as_mut() {
                        sign_into(device, signed);
                    }
                } else if let Some(master) = state.master.as_mut() {
                    sign_into(master, signed);
                }
            }
            ok(json!({"failures":{}}))
        } else if let Some(kind) = path
            .strip_prefix(&format!("/_matrix/client/v3/user/{USER}/account_data/"))
            .map(str::to_owned)
        {
            if http == "PUT" {
                state.account_data.insert(kind, body);
                ok(json!({}))
            } else {
                match state.account_data.get(&kind) {
                    Some(content) => ok(content.clone()),
                    None => not_found("M_NOT_FOUND"),
                }
            }
        } else if path == "/_matrix/client/v3/room_keys/version" {
            if http == "POST" {
                let mut backup = body.clone();
                backup["version"] = json!("1");
                backup["count"] = json!(0);
                backup["etag"] = json!("1");
                state.backup = Some(backup);
                ok(json!({"version":"1"}))
            } else {
                match &state.backup {
                    Some(backup) => ok(backup.clone()),
                    None => not_found("M_NOT_FOUND"),
                }
            }
        } else if path.starts_with("/_matrix/client/v3/room_keys/") {
            ok(json!({"etag":"1","count":0}))
        } else if path == "/_matrix/client/v3/sync" {
            state.batch += 1;
            let account_data: Vec<Value> = state
                .account_data
                .iter()
                .map(|(kind, content)| json!({"type":kind,"content":content}))
                .collect();
            ok(json!({
                "next_batch": format!("k{}", state.batch),
                "account_data": {"events": account_data},
                "to_device": {"events": []},
                "device_lists": {"changed": [USER], "left": []},
                "device_one_time_keys_count": {"signed_curve25519": 50}
            }))
        } else if path == "/_matrix/client/v3/logout" {
            ok(json!({}))
        } else {
            ResponseTemplate::new(404)
                .set_body_json(json!({"errcode":"M_UNRECOGNIZED","error":"Unrecognized request"}))
        }
    }
}

/// GIVEN an account whose encryption is not set up WHEN the page asks to
/// change the recovery key THEN nothing is asked. GIVEN a verified device
/// with recovery set up WHEN the user says no to the warning THEN the key
/// stays; WHEN they say yes THEN the core makes a new key, shows it in its
/// own window and writes it, alone, to the file chosen in its own Save
/// dialog. The key never appears in a reply to the page.
#[test]
fn given_recovery_then_the_key_changes_only_after_a_yes_and_is_saved_by_the_core() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(KeyServer::default())
            .mount(&server)
            .await;
        server
    });
    let dir = TempDir::new().unwrap();
    let saved = TempDir::new().unwrap();
    let answers = Answers::new();
    let core = Core::default()
        .with_credentials(answers.clone())
        .with_session(common::open_session(dir.path()));
    let _ = call(
        &core,
        Call::SignIn {
            homeserver: server.uri(),
        },
    );
    wait_for("sign-in", || {
        core.session().account_status().state == AccountState::SignedIn
    });
    wait_for("no identity", || {
        security(&core).identity == IdentityState::None
    });
    let refused = security(&core);
    let status: SecurityStatus = value(call(&core, Call::ChangeRecoveryKey {}));
    assert_eq!(status, refused, "not offered without recovery");
    assert_eq!(answers.changes_asked.load(Ordering::SeqCst), 0);

    // Set up encryption; the first key is saved where the dialog says.
    answers.answer(&[Ok(PASSWORD)]);
    *answers.save_to.lock().unwrap() = Some(saved.path().join("first.txt"));
    let _ = call(&core, Call::SetUpEncryption {});
    wait_for("recovery", || {
        let status = security(&core);
        status.operation == SecurityOperation::Idle
            && status.identity == IdentityState::Verified
            && status.recovery == RecoveryState::Enabled
    });
    wait_for("the first key", || answers.shown.lock().unwrap().len() == 1);
    let first = answers.shown.lock().unwrap()[0].clone();
    // The core holds the file open, unshared, until it has written it.
    wait_for("the first file", || {
        std::fs::read_to_string(saved.path().join("first.txt"))
            .is_ok_and(|text| text == format!("{first}\n"))
    });

    // "No" keeps the key.
    *answers.change.lock().unwrap() = Ok(false);
    let asked: SecurityStatus = value(call(&core, Call::ChangeRecoveryKey {}));
    assert_eq!(asked.operation, SecurityOperation::WaitingForConfirmation);
    wait_for("the answer", || {
        security(&core).operation == SecurityOperation::Idle
    });
    assert_eq!(security(&core).error, Some(SecurityError::Cancelled));
    assert_eq!(answers.shown.lock().unwrap().len(), 1, "no new key");

    // "Yes" makes, shows and saves a new one.
    *answers.change.lock().unwrap() = Ok(true);
    *answers.save_to.lock().unwrap() = Some(saved.path().join("second.txt"));
    let reply = call(&core, Call::ChangeRecoveryKey {});
    assert!(
        !serde_json::to_string(&reply).unwrap().contains(&first),
        "no key in the reply"
    );
    wait_for("the new key", || answers.shown.lock().unwrap().len() == 2);
    let second = answers.shown.lock().unwrap()[1].clone();
    assert_ne!(first, second);
    wait_for("the second file", || {
        std::fs::read_to_string(saved.path().join("second.txt"))
            .is_ok_and(|text| text == format!("{second}\n"))
    });
    wait_for("the change to finish", || {
        security(&core).operation == SecurityOperation::Idle
    });
    assert_eq!(security(&core).error, None);
    assert_eq!(answers.changes_asked.load(Ordering::SeqCst), 2);
}
