//! Registering a new account against a mock homeserver. The mock speaks plain
//! HTTP on loopback, which only the `dev-disposable-servers` feature accepts;
//! `tools/dev.py check` runs this file with it.
#![cfg(feature = "dev-disposable-servers")]

use persephone_contracts::{AccountState, AccountStatus, SignInError};
use persephone_core::{
    Core,
    credentials::{CredentialSource, Credentials, NewPassword, PromptError, SecretPrompt},
};
use persephone_core_protocol::{Call, ErrorCode, Outcome, Request};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path, query_param},
};
use zeroize::Zeroizing;

mod common;

const USERNAME: &str = "newbie";
const USER: &str = "@newbie:synthetic.test";
const PASSWORD: &str = "synthetic-new-password-Q4";
const REG_TOKEN: &str = "synthetic-reg-token";
const SESSION: &str = "uia-register-1";

/// Answers the registration prompts in order and records what each asked.
struct Scripted {
    answers: Mutex<Vec<Result<String, PromptError>>>,
    seen: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(answers: &[Result<&str, PromptError>]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(
                answers
                    .iter()
                    .map(|answer| answer.map(str::to_owned))
                    .collect(),
            ),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl CredentialSource for Scripted {
    fn ask(&self, _host: &str) -> Result<Credentials, PromptError> {
        panic!("registration never asks for sign-in credentials");
    }

    fn ask_secret(&self, prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        self.seen.lock().unwrap().push(match prompt {
            SecretPrompt::NewAccountPassword { username, step, .. } => {
                assert_eq!(username, USERNAME, "the prompt names the new account");
                format!("password:{step:?}")
            }
            SecretPrompt::RegistrationToken { .. } => "token".into(),
            other => panic!("unexpected prompt {other:?}"),
        });
        let mut answers = self.answers.lock().unwrap();
        assert!(!answers.is_empty(), "more prompts than answers");
        answers.remove(0).map(Zeroizing::new)
    }
}

struct Homeserver {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
}

impl Homeserver {
    /// `available`: the availability answer. `flows`: the registration flows
    /// offered. A token request with `REG_TOKEN` completes the token step;
    /// any other leaves it incomplete.
    fn start(available: (u16, Value), flows: Value) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/versions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"versions":["v1.11"]})))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/register/available"))
                .and(query_param("username", USERNAME))
                .respond_with(ResponseTemplate::new(available.0).set_body_json(available.1))
                .mount(&server)
                .await;
            let unauthorized = |completed: Value| {
                ResponseTemplate::new(401).set_body_json(json!({
                    "flows": flows.clone(), "params": {}, "session": SESSION, "completed": completed
                }))
            };
            // The dummy step after the token: the account is made.
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/register"))
                .and(body_partial_json(json!({
                    "username": USERNAME, "password": PASSWORD,
                    "auth": {"type": "m.login.dummy", "session": SESSION}
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "user_id": USER, "access_token": "syt_synthetic_new", "device_id": "PERSEPHONE2"
                })))
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/register"))
                .and(body_partial_json(json!({
                    "auth": {"type": "m.login.registration_token", "token": REG_TOKEN, "session": SESSION}
                })))
                .respond_with(unauthorized(json!(["m.login.registration_token"])))
                .with_priority(2)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/register"))
                .respond_with(unauthorized(json!([])))
                .with_priority(3)
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
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "next_batch": "classic-0", "rooms": {}, "account_data": {"events": []},
                    "to_device": {"events": []}, "device_lists": {"changed": [], "left": []},
                    "device_one_time_keys_count": {"signed_curve25519": 50}
                })))
                .mount(&server)
                .await;
            server
        });
        Self { runtime, server }
    }

    fn free(flows: Value) -> Self {
        Self::start((200, json!({"available": true})), flows)
    }

    fn uri(&self) -> String {
        self.server.uri()
    }

    fn requests_to(&self, wanted: &str) -> usize {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == wanted)
            .count()
    }
}

fn make_core(dir: &Path, source: Arc<dyn CredentialSource>) -> Core {
    Core::default()
        .with_credentials(source)
        .with_session(common::open_session(dir))
}

fn register(core: &Core, homeserver: &str, username: &str) -> Outcome {
    core.handle(&Request {
        id: 1,
        call: Call::Register {
            homeserver: homeserver.into(),
            username: username.into(),
        },
    })
    .outcome
}

fn wait_for_end(core: &Core) -> AccountStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = core.session().account_status();
        if status.state != AccountState::SigningIn {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for registration"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

const REGISTER: &str = "/_matrix/client/v3/register";

/// GIVEN a homeserver that needs a registration token WHEN registering THEN
/// the name is checked, the password is asked twice and the token once in
/// the core's prompts, the token and dummy steps follow, and the new account
/// is signed in.
#[test]
fn given_a_token_flow_then_the_account_is_registered_and_signed_in() {
    let server =
        Homeserver::free(json!([{"stages": ["m.login.registration_token", "m.login.dummy"]}]));
    let dir = TempDir::new().unwrap();
    let source = Scripted::new(&[Ok(PASSWORD), Ok(PASSWORD), Ok(REG_TOKEN)]);
    let core = make_core(dir.path(), source.clone());
    match register(&core, &server.uri(), USERNAME) {
        Outcome::Ok(value) => assert_eq!(value["state"], "signing_in"),
        other => panic!("{other:?}"),
    }
    let status = wait_for_end(&core);
    assert_eq!(status.state, AccountState::SignedIn, "{status:?}");
    assert_eq!(status.user_id.as_deref(), Some(USER));
    assert_eq!(
        source.seen(),
        ["password:Choose", "password:Confirm", "token"]
    );
    assert_eq!(
        server.requests_to("/_matrix/client/v3/register/available"),
        1
    );
    assert_eq!(server.requests_to(REGISTER), 3, "probe, token, dummy");
}

/// GIVEN a taken name WHEN registering THEN that is the reported reason, and
/// nothing is asked or registered.
#[test]
fn given_a_taken_username_then_it_is_reported_before_any_prompt() {
    let server = Homeserver::start(
        (400, json!({"errcode": "M_USER_IN_USE", "error": "taken"})),
        json!([{"stages": ["m.login.dummy"]}]),
    );
    let dir = TempDir::new().unwrap();
    let source = Scripted::new(&[]);
    let core = make_core(dir.path(), source.clone());
    register(&core, &server.uri(), USERNAME);
    let status = wait_for_end(&core);
    assert_eq!(status.state, AccountState::SignInFailed);
    assert_eq!(status.error, Some(SignInError::UsernameTaken));
    assert!(source.seen().is_empty());
    assert_eq!(server.requests_to(REGISTER), 0);
}

/// GIVEN two different password entries WHEN registering THEN the password
/// is asked again until both match, and a dummy-only flow needs no token.
#[test]
fn given_different_passwords_then_they_are_asked_again() {
    let server = Homeserver::free(json!([{"stages": ["m.login.dummy"]}]));
    let dir = TempDir::new().unwrap();
    let source = Scripted::new(&[Ok("one"), Ok("two"), Ok(PASSWORD), Ok(PASSWORD)]);
    let core = make_core(dir.path(), source.clone());
    register(&core, &server.uri(), USERNAME);
    let status = wait_for_end(&core);
    assert_eq!(status.state, AccountState::SignedIn, "{status:?}");
    assert_eq!(
        source.seen(),
        [
            "password:Choose",
            "password:Confirm",
            "password:Mismatch",
            "password:Confirm"
        ]
    );
}

/// GIVEN a token the homeserver does not accept, a flow with a step this
/// client cannot take, or a cancelled prompt THEN each is reported by its
/// own code and no account is signed in.
#[test]
fn given_refusals_then_each_reason_is_reported() {
    let token_flow = json!([{"stages": ["m.login.registration_token", "m.login.dummy"]}]);
    for (flows, answers, expected) in [
        (
            token_flow.clone(),
            vec![Ok(PASSWORD), Ok(PASSWORD), Ok("wrong-token")],
            SignInError::TokenRejected,
        ),
        (
            json!([{"stages": ["m.login.recaptcha"]}, {"stages": ["m.login.email.identity"]}]),
            vec![Ok(PASSWORD), Ok(PASSWORD)],
            SignInError::UnsupportedRegistration,
        ),
        (
            token_flow,
            vec![Ok(PASSWORD), Ok(PASSWORD), Err(PromptError::Cancelled)],
            SignInError::Cancelled,
        ),
    ] {
        let server = Homeserver::free(flows);
        let dir = TempDir::new().unwrap();
        let core = make_core(dir.path(), Scripted::new(&answers));
        register(&core, &server.uri(), USERNAME);
        let status = wait_for_end(&core);
        assert_eq!(status.state, AccountState::SignInFailed, "{expected:?}");
        assert_eq!(status.error, Some(expected));
        assert_eq!(status.user_id, None);
    }
}

/// GIVEN a username that is not a valid localpart, or a bad homeserver URL
/// THEN the call is refused before anything is asked or sent.
#[test]
fn given_bad_arguments_then_registration_is_refused_before_any_request() {
    let server = Homeserver::free(json!([{"stages": ["m.login.dummy"]}]));
    let dir = TempDir::new().unwrap();
    let source = Scripted::new(&[]);
    let core = make_core(dir.path(), source.clone());
    for (homeserver, username) in [
        (server.uri(), "Newbie".to_owned()),
        (server.uri(), "@newbie:synthetic.test".to_owned()),
        (server.uri(), String::new()),
        (server.uri(), "a".repeat(256)),
        ("ftp://synthetic.test".to_owned(), USERNAME.to_owned()),
    ] {
        assert_eq!(
            register(&core, &homeserver, &username),
            Outcome::Error(ErrorCode::InvalidArgument),
            "{homeserver} {username}"
        );
    }
    assert!(source.seen().is_empty());
    assert_eq!(
        server.requests_to("/_matrix/client/v3/register/available"),
        0
    );
    let _ = NewPassword::Choose;
}
