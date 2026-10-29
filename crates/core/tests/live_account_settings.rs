//! Live check of the account settings against a disposable homeserver from
//! `tools/servers/run.py`. Ignored by default: run it with the runner's
//! profile in `PERSEPHONE_LIVE_PROFILE` and the `dev-disposable-servers`
//! feature (plain HTTP to loopback). See `docs/evidence/account-settings.md`.
//!
//! The core under test signs in as the profile's first account. A plain SDK
//! client signs in as the recipient to send a message, and another as the
//! same account to be the second device. Nothing here reads a production
//! profile; output carries codes and phases, never secrets.
#![cfg(feature = "dev-disposable-servers")]

use matrix_sdk::{
    Client,
    config::SyncSettings,
    ruma::{
        OwnedUserId, api::client::room::create_room, events::room::message::RoomMessageEventContent,
    },
};
use persephone_contracts::{
    AccountChange, AccountSettings, AccountState, DeviceList, IdentityState, InviteList,
    NotificationSettings, RecoveryState, RoomList, SecurityOperation, SecurityStatus, ThreePidList,
    VerificationPhase,
};
use persephone_core::{
    Core,
    credentials::{CredentialSource, Credentials, NewPassword, PromptError, SecretPrompt},
};
use persephone_core_protocol::{Call, Outcome, Request};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

mod common;

struct Account {
    base_url: String,
    username: String,
    password: String,
    recipient: (String, String),
}

fn account() -> Account {
    let path = std::env::var("PERSEPHONE_LIVE_PROFILE").expect("PERSEPHONE_LIVE_PROFILE");
    let profiles: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let first = &profiles[0];
    Account {
        base_url: first["base_url"].as_str().unwrap().to_owned(),
        username: first["username"].as_str().unwrap().to_owned(),
        password: first["password"].as_str().unwrap().to_owned(),
        recipient: (
            first["recipient"]["username"].as_str().unwrap().to_owned(),
            first["recipient"]["password"].as_str().unwrap().to_owned(),
        ),
    }
}

/// Answers every prompt from the profile: the current password, a new one,
/// yes to the recovery-key change, and a `.local` file for the Save dialog.
struct Live {
    username: String,
    password: Mutex<String>,
    new_password: String,
    save_to: PathBuf,
    shown: Mutex<Vec<String>>,
}

impl CredentialSource for Live {
    fn ask(&self, _host: &str) -> Result<Credentials, PromptError> {
        Ok(Credentials {
            username: Zeroizing::new(self.username.clone()),
            password: Zeroizing::new(self.password.lock().unwrap().clone()),
        })
    }

    fn ask_secret(&self, prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        Ok(Zeroizing::new(match prompt {
            SecretPrompt::ChangedPassword {
                step: NewPassword::Choose | NewPassword::Confirm | NewPassword::Mismatch,
                ..
            } => self.new_password.clone(),
            _ => self.password.lock().unwrap().clone(),
        }))
    }

    fn show_recovery_key(&self, _user_id: &str, key: &str) -> Result<(), PromptError> {
        self.shown.lock().unwrap().push(key.to_owned());
        Ok(())
    }

    fn confirm_recovery_key_change(&self) -> Result<bool, PromptError> {
        Ok(true)
    }

    fn choose_recovery_key_file(&self) -> Result<Option<PathBuf>, PromptError> {
        let shown = self.shown.lock().unwrap().len();
        Ok(Some(self.save_to.join(format!("recovery-key-{shown}.txt"))))
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

fn wait_for(what: &str, seconds: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn security(core: &Core) -> SecurityStatus {
    value(call(core, Call::SecurityStatus {}))
}

async fn sdk_client(base: &str, user: &str, password: &str, device: &str) -> Client {
    let client = Client::builder()
        .homeserver_url(base)
        .build()
        .await
        .unwrap();
    client
        .matrix_auth()
        .login_username(user, password)
        .initial_device_display_name(device)
        .send()
        .await
        .unwrap();
    client
}

#[test]
#[ignore = "needs a disposable homeserver profile in PERSEPHONE_LIVE_PROFILE"]
fn live_account_settings() {
    let live = account();
    let local =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/account-settings-live");
    std::fs::create_dir_all(&local).unwrap();
    let profile = tempfile::TempDir::new_in(&local).unwrap();
    let source = Arc::new(Live {
        username: live.username.clone(),
        password: Mutex::new(live.password.clone()),
        new_password: format!("{}Zz9!", live.password),
        save_to: profile.path().to_path_buf(),
        shown: Mutex::default(),
    });
    let core = Core::default()
        .with_credentials(source.clone())
        .with_session(common::open_session(profile.path().join("profile")));
    let _ = call(
        &core,
        Call::SignIn {
            homeserver: live.base_url.clone(),
        },
    );
    wait_for("sign-in", 60, || {
        core.session().account_status().state == AccountState::SignedIn
    });
    let user_id = core.session().account_status().user_id.unwrap();
    println!("RESULT sign_in signed_in");

    // Notification level: read, set, read back after a sync.
    let level: NotificationSettings = value(call(&core, Call::NotificationLevel {}));
    println!("RESULT notification_level_read {:?}", level.level);
    for set in [
        persephone_contracts::NotificationLevel::Mentions,
        persephone_contracts::NotificationLevel::None,
        persephone_contracts::NotificationLevel::All,
    ] {
        let outcome = call(&core, Call::SetNotificationLevel { level: set });
        println!("RESULT notification_level_set {set:?} {outcome:?}");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut read = None;
        while Instant::now() < deadline {
            let now: NotificationSettings = value(call(&core, Call::NotificationLevel {}));
            read = Some(now.level);
            if now.level == set {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        println!("RESULT notification_level_reads_back {set:?} {read:?}");
    }

    // Email addresses and phone numbers.
    let listed: ThreePidList = value(call(&core, Call::ThreePids {}));
    println!(
        "RESULT three_pids listed={} can_change={}",
        listed.three_pids.len(),
        listed.can_change
    );
    let email: AccountChange = value(call(
        &core,
        Call::RequestEmailToken {
            email: "p0_alice@example.invalid".into(),
        },
    ));
    println!("RESULT request_email_token {email:?}");
    let phone: AccountChange = value(call(
        &core,
        Call::RequestPhoneToken {
            country: "GB".into(),
            phone_number: "07700900123".into(),
        },
    ));
    println!("RESULT request_phone_token {phone:?}");
    let add: AccountChange = value(call(&core, Call::AddThreePid {}));
    println!("RESULT add_three_pid {add:?}");
    let remove: AccountChange = value(call(
        &core,
        Call::RemoveThreePid {
            medium: persephone_contracts::ThreePidMedium::Email,
            address: "p0_alice@example.invalid".into(),
        },
    ));
    println!("RESULT remove_three_pid {remove:?}");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    // The account's second device: it joins the test room, then is verified.
    let second = runtime.block_on(async {
        let client = sdk_client(
            &live.base_url,
            &live.username,
            &live.password,
            "live-second",
        )
        .await;
        client.sync_once(SyncSettings::default()).await.unwrap();
        client
    });

    // Desktop notifications: the recipient invites this account and writes.
    let _ = call(
        &core,
        Call::SetAccountSettings {
            settings: AccountSettings {
                chat_text_percent: 110,
                interface_text_percent: 100,
                desktop_notifications: true,
                ..AccountSettings::default()
            },
        },
    );
    let bob = runtime.block_on(sdk_client(
        &live.base_url,
        &live.recipient.0,
        &live.recipient.1,
        "live-recipient",
    ));
    let alice_id: OwnedUserId = user_id.parse().unwrap();
    let room = runtime.block_on(async {
        let mut request = create_room::v3::Request::new();
        request.name = Some("Live notifications".into());
        request.invite = vec![alice_id.clone()];
        bob.create_room(request).await.unwrap()
    });
    let room_id = room.room_id().to_string();
    // Seen by the core as an invitation? Reported, not required: the
    // account's second device joins the room either way.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut invited = false;
    while !invited && Instant::now() < deadline {
        let invites: InviteList = value(call(&core, Call::InviteList {}));
        invited = invites
            .invites
            .iter()
            .any(|invite| invite.room_id == room_id);
        std::thread::sleep(Duration::from_millis(300));
    }
    println!("RESULT invitation_listed {invited}");
    runtime.block_on(async {
        second.sync_once(SyncSettings::default()).await.unwrap();
        second.join_room_by_id(room.room_id()).await.unwrap();
    });
    wait_for("the joined room", 60, || {
        let rooms: RoomList = value(call(&core, Call::RoomList {}));
        rooms.rooms.iter().any(|room| room.room_id == room_id)
    });
    runtime.block_on(async {
        bob.sync_once(SyncSettings::default()).await.unwrap();
        let room = bob.get_room(room.room_id()).unwrap();
        room.send(RoomMessageEventContent::text_plain("live secret text"))
            .await
            .unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while core.pending_toast().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    match core.pending_toast() {
        Some((room, sender)) => println!(
            "RESULT toast room={room:?} sender={sender:?} has_text={}",
            room.contains("secret") || sender.contains("secret")
        ),
        None => println!("RESULT toast none"),
    }

    // Encryption, then a new recovery key saved by the core.
    wait_for("the identity state", 60, || {
        security(&core).identity == IdentityState::None
    });
    let _ = call(&core, Call::SetUpEncryption {});
    wait_for("recovery", 120, || {
        let status = security(&core);
        status.operation == SecurityOperation::Idle
            && status.identity == IdentityState::Verified
            && status.recovery == RecoveryState::Enabled
    });
    wait_for("the first key", 30, || {
        source.shown.lock().unwrap().len() == 1
    });
    let _ = call(&core, Call::ChangeRecoveryKey {});
    wait_for("the new key", 120, || {
        source.shown.lock().unwrap().len() == 2
    });
    wait_for("the change", 60, || {
        security(&core).operation == SecurityOperation::Idle
    });
    let keys = source.shown.lock().unwrap().clone();
    let saved = std::fs::read_to_string(profile.path().join("recovery-key-2.txt")).unwrap();
    println!(
        "RESULT recovery_key_change error={:?} new_key_differs={} saved_matches={}",
        security(&core).error,
        keys[0] != keys[1],
        saved == format!("{}\n", keys[1])
    );

    // Verify the second device of this account from the core.
    let second_id = second.device_id().unwrap().to_string();
    wait_for("the second device in the list", 60, || {
        let devices: DeviceList = value(call(&core, Call::DeviceList {}));
        devices
            .devices
            .iter()
            .any(|device| device.device_id == second_id)
    });
    let started: SecurityStatus = value(call(
        &core,
        Call::VerifyDevice {
            device_id: second_id.clone(),
        },
    ));
    println!("RESULT verify_device_started {:?}", started.operation);
    wait_for("the flow", 60, || security(&core).verification.is_some());
    let flow_id = security(&core).verification.unwrap().flow_id;
    let sas = runtime.block_on(async {
        let alice: OwnedUserId = user_id.parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            second.sync_once(SyncSettings::default()).await.unwrap();
            if let Some(request) = second
                .encryption()
                .get_verification_request(&alice, &flow_id)
                .await
            {
                request.accept().await.unwrap();
                break request;
            }
            assert!(Instant::now() < deadline, "the request reached the device");
        }
    });
    let sas = runtime.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            second.sync_once(SyncSettings::default()).await.unwrap();
            if let Some(verification) = sas.start_sas().await.ok().flatten() {
                break verification;
            }
            assert!(Instant::now() < deadline, "the comparison started");
        }
    });
    wait_for("the comparison", 60, || {
        let _ = runtime.block_on(second.sync_once(SyncSettings::default()));
        security(&core)
            .verification
            .is_some_and(|flow| flow.phase == VerificationPhase::Compare)
    });
    let ours: Vec<String> = security(&core)
        .verification
        .unwrap()
        .emoji
        .into_iter()
        .map(|emoji| emoji.symbol)
        .collect();
    let theirs: Vec<String> = sas
        .emoji()
        .unwrap()
        .iter()
        .map(|emoji| emoji.symbol.to_owned())
        .collect();
    println!("RESULT verify_device_emoji_match {}", ours == theirs);
    let _ = call(
        &core,
        Call::ConfirmVerification {
            flow_id: flow_id.clone(),
        },
    );
    runtime.block_on(sas.confirm()).unwrap();
    wait_for("done", 60, || {
        let _ = runtime.block_on(second.sync_once(SyncSettings::default()));
        security(&core)
            .verification
            .is_some_and(|flow| flow.phase == VerificationPhase::Done)
    });
    let devices: DeviceList = value(call(&core, Call::DeviceList {}));
    let verified = devices
        .devices
        .iter()
        .find(|device| device.device_id == second_id)
        .is_some_and(|device| device.verified);
    println!("RESULT verify_device_done listed_verified={verified}");

    // The password, last: a change keeping other sessions, then a wrong one.
    let changed: AccountChange = value(call(
        &core,
        Call::ChangePassword {
            sign_out_others: false,
        },
    ));
    println!("RESULT change_password {changed:?}");
    let still = runtime.block_on(second.whoami()).is_ok();
    println!("RESULT other_session_kept {still}");
    *source.password.lock().unwrap() = "not-the-password".into();
    let wrong: AccountChange = value(call(
        &core,
        Call::ChangePassword {
            sign_out_others: true,
        },
    ));
    println!("RESULT change_password_wrong {wrong:?}");
    // With the right current password, other sessions are signed out.
    *source.password.lock().unwrap() = format!("{}Zz9!", live.password);
    let again: AccountChange = value(call(
        &core,
        Call::ChangePassword {
            sign_out_others: true,
        },
    ));
    let gone = runtime.block_on(second.whoami()).is_err();
    println!("RESULT change_password_sign_out {again:?} other_session_signed_out={gone}");
    let _ = call(&core, Call::SignOut {});
    drop(second);
    drop(bob);
}
