//! Emoji verification between two devices of one account, with the real SDK on
//! both sides and a small in-memory homeserver: it keeps device and
//! cross-signing keys, relays to-device messages, and answers `/sync` with
//! them. Plain HTTP on loopback, used by these test clients only.

use matrix_sdk::{
    Client, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::verification::{SasVerification, VerificationRequest},
    ruma::{OwnedUserId, UserId, user_id},
};
use persephone_contracts::VerificationPhase;
use persephone_core::verification::{self, Flow};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::any};

const USER: &str = "@alice:synthetic.test";
/// This device. Its ID sorts after the other device's, so when both start a
/// comparison at once, the SDK keeps the other device's start (the start of
/// the lexicographically smaller device wins).
const THIS: &str = "PERSEPHONE";
const OTHER: &str = "ELEMENT";

#[derive(Default)]
struct State {
    devices: BTreeMap<String, Value>,
    master: Option<Value>,
    self_signing: Option<Value>,
    user_signing: Option<Value>,
    /// To-device events waiting for each device's next `/sync`.
    inbox: BTreeMap<String, Vec<Value>>,
    one_time_keys: BTreeMap<String, usize>,
    batch: u64,
}

/// Answers every request from [`State`]. One user; the access token names
/// the device.
#[derive(Clone, Default)]
struct Homeserver(Arc<Mutex<State>>);

fn device_of(request: &Request) -> String {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer token-"))
        .unwrap_or_default()
        .to_owned()
}

/// Adds the signatures of `signed` to `stored`.
fn merge_signatures(stored: &mut Value, signed: &Value) {
    let Some(new) = signed["signatures"].as_object() else {
        return;
    };
    let signatures = stored["signatures"]
        .as_object_mut()
        .expect("uploaded keys are signed");
    for (signer, keys) in new {
        let entry = signatures
            .entry(signer.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        for (key, signature) in keys.as_object().into_iter().flatten() {
            entry[key] = signature.clone();
        }
    }
}

impl Respond for Homeserver {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let device = device_of(request);
        let mut state = self.0.lock().unwrap();
        let reply = if path.ends_with("/versions") {
            json!({ "versions": ["v1.19"] })
        } else if path.ends_with("/keys/upload") {
            if let Some(keys) = body.get("device_keys") {
                state.devices.insert(device.clone(), keys.clone());
            }
            let count = state.one_time_keys.entry(device).or_default();
            *count += body["one_time_keys"].as_object().map_or(0, Map::len);
            json!({ "one_time_key_counts": { "signed_curve25519": *count } })
        } else if path.ends_with("/keys/query") {
            let mut reply = json!({
                "device_keys": { USER: state.devices },
                "failures": {},
            });
            for (field, key) in [
                ("master_keys", &state.master),
                ("self_signing_keys", &state.self_signing),
                ("user_signing_keys", &state.user_signing),
            ] {
                if let Some(key) = key {
                    reply[field] = json!({ USER: key });
                }
            }
            reply
        } else if path.ends_with("/keys/device_signing/upload") {
            state.master = body.get("master_key").cloned();
            state.self_signing = body.get("self_signing_key").cloned();
            state.user_signing = body.get("user_signing_key").cloned();
            json!({})
        } else if path.ends_with("/keys/signatures/upload") {
            for (key_id, signed) in body[USER].as_object().into_iter().flatten() {
                if let Some(stored) = state.devices.get_mut(key_id) {
                    merge_signatures(stored, signed);
                } else if let Some(master) = state.master.as_mut()
                    && master["keys"]
                        .as_object()
                        .is_some_and(|keys| keys.values().any(|key| key == key_id))
                {
                    merge_signatures(master, signed);
                }
            }
            json!({ "failures": {} })
        } else if path.contains("/sendToDevice/") {
            let event_type = path.split('/').rev().nth(1).unwrap_or_default().to_owned();
            for (target, content) in body["messages"][USER].as_object().into_iter().flatten() {
                let targets: Vec<String> = if target == "*" {
                    state
                        .devices
                        .keys()
                        .filter(|id| **id != device)
                        .cloned()
                        .collect()
                } else {
                    vec![target.clone()]
                };
                for target in targets {
                    state.inbox.entry(target).or_default().push(json!({
                        "sender": USER,
                        "type": event_type,
                        "content": content,
                    }));
                }
            }
            json!({})
        } else if path.ends_with("/sync") {
            state.batch += 1;
            let events = state.inbox.remove(&device).unwrap_or_default();
            json!({
                "next_batch": format!("s{}", state.batch),
                "to_device": { "events": events },
                "device_lists": { "changed": [USER], "left": [] },
                "device_one_time_keys_count": { "signed_curve25519": 50 },
            })
        } else if path.contains("/account_data/") {
            // Nothing stored: Matrix v1.19 answers 404 `M_NOT_FOUND`.
            return ResponseTemplate::new(404).set_body_json(
                json!({ "errcode": "M_NOT_FOUND", "error": "Account data not found." }),
            );
        } else {
            // An endpoint this server does not implement.
            return ResponseTemplate::new(404).set_body_json(
                json!({ "errcode": "M_UNRECOGNIZED", "error": "Unrecognized request" }),
            );
        };
        ResponseTemplate::new(200).set_body_json(reply)
    }
}

async fn signed_in(server: &MockServer, device: &str) -> Client {
    let client = Client::builder()
        .homeserver_url(server.uri())
        .build()
        .await
        .unwrap();
    client
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@alice:synthetic.test").to_owned(),
                device_id: device.into(),
            },
            tokens: SessionTokens {
                access_token: format!("token-{device}"),
                refresh_token: None,
            },
        })
        .await
        .unwrap();
    client
}

/// One sync round: sends the device's queued requests, then takes its
/// to-device events.
async fn sync(client: &Client) {
    client.sync_once(SyncSettings::default()).await.unwrap();
}

fn user() -> OwnedUserId {
    UserId::parse(USER).unwrap()
}

/// This device, the other device, this device's flow asking the account to
/// verify it, and the other device's copy of that request, received but not
/// yet answered.
async fn requested(server: &MockServer) -> (Client, Client, Flow, VerificationRequest) {
    Mock::given(any())
        .respond_with(Homeserver::default())
        .mount(server)
        .await;
    let this = signed_in(server, THIS).await;
    let other = signed_in(server, OTHER).await;

    // Both devices upload their keys; this one sets up cross-signing, and
    // each learns the other's keys.
    sync(&this).await;
    sync(&other).await;
    this.encryption()
        .bootstrap_cross_signing(None)
        .await
        .unwrap();
    for _ in 0..2 {
        sync(&this).await;
        sync(&other).await;
    }

    let flow = verification::start(&this, &user())
        .await
        .expect("the request is sent");
    sync(&other).await;
    let request = other
        .encryption()
        .get_verification_request(&user(), flow.flow_id())
        .await
        .expect("the other device received the request");
    (this, other, flow, request)
}

/// Syncs both devices, driving the flow after each of this device's rounds,
/// until both show emoji, then checks that the flow shows the other device's.
async fn assert_compares(this: &Client, other: &Client, flow: &mut Flow, theirs: &SasVerification) {
    for _ in 0..6 {
        sync(this).await;
        flow.drive().await;
        sync(other).await;
        if flow.view().phase == VerificationPhase::Compare && theirs.emoji().is_some() {
            break;
        }
    }
    let view = flow.view();
    assert_eq!(view.phase, VerificationPhase::Compare);
    let ours: Vec<&str> = view.emoji.iter().map(|item| item.symbol.as_str()).collect();
    let expected: Vec<&str> = theirs
        .emoji()
        .expect("the other device has emoji")
        .iter()
        .map(|item| item.symbol)
        .collect();
    assert_eq!(ours, expected);
}

/// GIVEN this device asked the account's other device to verify it WHEN the
/// other device answers ready and starts the emoji comparison before this
/// device has seen the ready THEN the flow accepts that comparison and shows
/// the same seven emoji as the other device.
#[test]
fn given_the_other_device_starts_the_comparison_then_the_flow_accepts_it() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let server = MockServer::start().await;
        let (this, other, mut flow, request) = requested(&server).await;
        request.accept().await.unwrap();
        let theirs = request
            .start_sas()
            .await
            .unwrap()
            .expect("the other device starts a comparison");

        assert_compares(&this, &other, &mut flow, &theirs).await;
    });
}

/// GIVEN this device asked the account's other device to verify it, and both
/// devices start the emoji comparison before either has seen the other's
/// start WHEN the SDK keeps the other device's start and replaces ours THEN
/// the flow follows the comparison that runs: it accepts it and shows the
/// same seven emoji as the other device.
#[test]
fn given_both_devices_start_a_comparison_then_the_flow_follows_the_one_kept() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let server = MockServer::start().await;
        let (this, other, mut flow, request) = requested(&server).await;
        request.accept().await.unwrap();
        sync(&this).await;

        // Both start: this flow on the other device's ready, the other device
        // by hand, before either start arrives.
        flow.drive().await;
        let theirs = request
            .start_sas()
            .await
            .unwrap()
            .expect("the other device starts a comparison");

        assert_compares(&this, &other, &mut flow, &theirs).await;
    });
}

/// This device with cross-signing set up (so it holds the self-signing key)
/// and the account's other device, each knowing the other's keys.
async fn two_devices(server: &MockServer) -> (Client, Client) {
    Mock::given(any())
        .respond_with(Homeserver::default())
        .mount(server)
        .await;
    let this = signed_in(server, THIS).await;
    let other = signed_in(server, OTHER).await;
    sync(&this).await;
    sync(&other).await;
    this.encryption()
        .bootstrap_cross_signing(None)
        .await
        .unwrap();
    for _ in 0..2 {
        sync(&this).await;
        sync(&other).await;
    }
    (this, other)
}

/// GIVEN this device holds the account's self-signing key WHEN it asks the
/// account's other device, by its ID, to verify each other and both confirm
/// the same emoji THEN the request reaches that device, the flow ends done,
/// and this device's store holds the other device as signed by the account.
#[test]
fn given_a_verified_device_then_it_verifies_another_device_by_id() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let server = MockServer::start().await;
        let (this, other) = two_devices(&server).await;
        let mut flow = verification::start_device(&this, &user(), OTHER)
            .await
            .expect("the request is sent to the other device");
        assert!(!flow.is_contact(), "it verifies the account's own device");
        sync(&other).await;
        let request = other
            .encryption()
            .get_verification_request(&user(), flow.flow_id())
            .await
            .expect("the other device received the request");
        request.accept().await.unwrap();
        let theirs = request
            .start_sas()
            .await
            .unwrap()
            .expect("the other device starts a comparison");
        assert_compares(&this, &other, &mut flow, &theirs).await;

        let id = flow.flow_id().to_owned();
        flow.act(&this, &verification::Action::Confirm(id))
            .await
            .unwrap();
        theirs.confirm().await.unwrap();
        for _ in 0..6 {
            sync(&this).await;
            flow.drive().await;
            sync(&other).await;
            if flow.view().phase == VerificationPhase::Done {
                break;
            }
        }
        assert_eq!(flow.view().phase, VerificationPhase::Done);
        let device = this
            .encryption()
            .get_device(&user(), &matrix_sdk::ruma::OwnedDeviceId::from(OTHER))
            .await
            .unwrap()
            .expect("the other device is known");
        assert!(device.is_verified(), "this device trusts the other one now");
    });
}

/// GIVEN this device WHEN it asks to verify itself, a device the account
/// does not have, or another device while it lacks the self-signing key
/// THEN each is refused with its own code and no request is sent.
#[test]
fn given_a_device_this_device_cannot_verify_then_it_is_refused() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let server = MockServer::start().await;
        let (this, other) = two_devices(&server).await;
        for (device, expected) in [
            (THIS, persephone_contracts::SecurityError::DeviceNotFound),
            (
                "NOSUCHDEVICE",
                persephone_contracts::SecurityError::DeviceNotFound,
            ),
        ] {
            assert_eq!(
                verification::start_device(&this, &user(), device)
                    .await
                    .err(),
                Some(expected),
                "{device}"
            );
        }
        // The other device never received the private cross-signing keys.
        assert_eq!(
            verification::start_device(&other, &user(), THIS)
                .await
                .err(),
            Some(persephone_contracts::SecurityError::CannotSignDevices)
        );
    });
}
