//! P4 live peer: the harness profile's second account ("Bob"), driven one
//! command per line on stdin by a test driver while Persephone runs as the
//! first account. Each command prints one JSON line. Synthetic disposable
//! servers only; nothing secret is printed.
//!
//! Commands:
//! * `login bob` / `login bob2`: sign in. `bob` follows Persephone's policy
//!   (ADR-0005); `bob2` is a second device of the same account that is never
//!   cross-signed and uses the SDK's default key sharing.
//! * `bootstrap`: set up cross-signing for `bob`, answering the password round.
//!   With keys already on this device it uploads the same identity again.
//! * `reset-identity [<user id>]`: replace `bob`'s cross-signing identity with
//!   a new one (as after losing every device), answering the password round.
//!   Bob's new identity has not signed anyone, so a user he had verified now
//!   reads as a changed verified user and his client refuses to send to them;
//!   naming that user withdraws the old verification, as a user would.
//! * `create <encrypted|plain|public> <user id> <name>`: `bob` creates a
//!   room with that name and invites the user: private and encrypted,
//!   private without encryption state (some servers encrypt it anyway), or
//!   public without encryption.
//! * `send <bob|bob2> <room id> <text>`
//! * `expect <bob|bob2> <room id> <seconds> <text>`: waits for a message with
//!   exactly this text from someone else and reports whether it arrived and
//!   whether it arrived encrypted.
//! * `login alice`: sign in as the first account on a device of its own,
//!   then `recover <file>` with the recovery key in that file (under
//!   `.local`), so this device holds the account's cross-signing keys.
//! * `wait-new-device <seconds>`: sync until this device knows another of the
//!   account's devices that it did not know before, and print its ID. A real
//!   other device syncs all the time; the SDK drops a request from a device
//!   it does not know yet, so a test waits for this before a request is sent.
//! * `verify-accept <seconds>`: wait for a verification request from another
//!   of the account's devices, accept it and the comparison it starts, and
//!   print the emoji once they can be compared.
//! * `verify-request <device id> <seconds>`: ask that device of the account
//!   to verify (a device that is not cross-signed yet gets a request only
//!   when it is named), start the comparison once it is ready, and print the
//!   emoji.
//! * `verify-confirm`, `verify-mismatch`, `verify-wait <seconds>`: for the
//!   last flow either device or Bob started.
//! * `create dm <user id>`: `bob` creates an encrypted direct-message room
//!   with that user and marks it as direct for himself.
//! * `alice-join-dm <room id>`: Alice's own device (`login alice`) joins that
//!   room for the account and marks it as direct, so Persephone sees it as a
//!   direct-message room with Bob.
//! * `contact-accept <seconds>`: `bob` waits for a verification request
//!   from Alice in a direct-message room, accepts it and the comparison it
//!   starts, and prints the emoji.
//! * `contact-request <user id> <seconds>`: `bob` asks that user to verify
//!   in their existing direct-message room, starts the comparison once they
//!   are ready, and prints the emoji.
//! * `trust <bob|alice> <user id>`: whether that client sees the user's
//!   identity as verified.
//! * `count <bob|bob2> <room id> <seconds> <text>`: sync for that long, then
//!   count the messages with exactly this text from someone else (decrypted
//!   ones only) and the ones it cannot decrypt, with their reasons.
//! * `sign-devices`: `bob` signs each of his own devices that his identity
//!   has not signed yet (such as `bob2`), as accepting a new session does.
//! * `wait-devices <bob|alice> <user id> <count> <seconds>`: sync until that
//!   client knows at least `count` devices of the user. The SDK drops a
//!   request from a device it does not know yet, so a test waits for this.
//! * `quit`

use anyhow::{Context, Result, bail};
use matrix_sdk::{
    Client,
    config::{RequestConfig, SyncSettings},
    deserialized_responses::TimelineEventKind,
    room::MessagesOptions,
    ruma::{
        OwnedDeviceId, OwnedRoomId, OwnedUserId, UInt,
        api::client::{
            room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
            uiaa::{AuthData, MatrixUserIdentifier, Password, UserIdentifier},
        },
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, InitialStateEvent, SyncMessageLikeEvent,
            room::{
                encryption::RoomEncryptionEventContent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
    },
};
use matrix_sdk::{
    encryption::CrossSigningResetAuthType,
    encryption::verification::{
        SasState, SasVerification, VerificationRequest, VerificationRequestState,
    },
    ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent,
};
use matrix_sdk_crypto::{CollectStrategy, DecryptionSettings, TrustRequirement};
use persephone_test_matrix::policy_http_client_builder;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashSet,
    io::BufRead,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const STACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct Account {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    base_url: String,
    username: String,
    password: String,
    recipient: Account,
}

struct Peer {
    base_url: String,
    account: Account,
    first: Account,
    bob: Option<Client>,
    bob2: Option<Client>,
    alice: Option<Client>,
    /// The newest verification request another of Alice's devices sent.
    requested: Arc<Mutex<Option<String>>>,
    /// The newest verification request someone else sent Bob in a room:
    /// sender and flow ID (the request's event ID).
    contact_requested: Arc<Mutex<Option<(OwnedUserId, String)>>>,
    /// The client of the last flow, for `verify-wait`.
    verifier: Option<Client>,
    request: Option<VerificationRequest>,
    sas: Option<SasVerification>,
    /// Alice's devices this device knew when last asked.
    known: HashSet<OwnedDeviceId>,
}

async fn own_devices(client: &Client) -> Result<HashSet<OwnedDeviceId>> {
    let own = client.user_id().context("signed in")?;
    Ok(client
        .encryption()
        .get_user_devices(own)
        .await?
        .devices()
        .map(|device| device.device_id().to_owned())
        .collect())
}

fn emoji(sas: &SasVerification) -> Vec<String> {
    sas.emoji()
        .map(|emoji| {
            emoji
                .iter()
                .map(|item| item.description.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

async fn build(base_url: &str, policy: bool) -> Result<Client> {
    let mut builder = Client::builder()
        .homeserver_url(base_url)
        .respect_login_well_known(false)
        .disable_well_known_lookup(true)
        .http_client(policy_http_client_builder().build()?)
        .request_config(
            RequestConfig::new()
                .disable_retry()
                .timeout(Duration::from_secs(30)),
        );
    if policy {
        builder = builder
            .with_room_key_recipient_strategy(CollectStrategy::IdentityBasedStrategy)
            .with_decryption_settings(DecryptionSettings {
                sender_device_trust_requirement: TrustRequirement::CrossSignedOrLegacy,
            });
    }
    Ok(builder.build().await?)
}

impl Peer {
    fn client(&self, which: &str) -> Result<&Client> {
        match which {
            "bob" => self.bob.as_ref(),
            "bob2" => self.bob2.as_ref(),
            "alice" => self.alice.as_ref(),
            _ => None,
        }
        .context("unknown or signed-out client")
    }

    async fn sync_all(&self) {
        for client in [&self.bob, &self.bob2, &self.alice].into_iter().flatten() {
            let _ = client
                .sync_once(SyncSettings::default().timeout(Duration::from_millis(500)))
                .await;
        }
    }

    async fn run(&mut self, line: &str) -> Result<serde_json::Value> {
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or_default();
        self.sync_all().await;
        match command {
            "login" => {
                let which = words.next().context("which client")?;
                // Checked before signing in: replacing a client would leave its
                // session signed in, since `quit` logs out only the kept ones.
                let taken = match which {
                    "bob" => self.bob.is_some(),
                    "bob2" => self.bob2.is_some(),
                    "alice" => self.alice.is_some(),
                    _ => bail!("login bob, bob2 or alice"),
                };
                if taken {
                    bail!("{which} is already signed in");
                }
                let client = build(&self.base_url, which != "bob2").await?;
                let account = if which == "alice" {
                    &self.first
                } else {
                    &self.account
                };
                client
                    .matrix_auth()
                    .login_username(&account.username, &account.password)
                    .initial_device_display_name(match which {
                        "bob" => "P4 peer",
                        "alice" => "P4 alice",
                        _ => "P4 unsigned",
                    })
                    .await?;
                client
                    .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
                    .await?;
                let reply = json!({"ok": true, "user": client.user_id().map(|u| u.to_string())});
                match which {
                    "bob" => {
                        let recorder = self.contact_requested.clone();
                        let own = client.user_id().context("signed in")?.to_owned();
                        client.add_event_handler(move |event: OriginalSyncRoomMessageEvent| {
                            if let MessageType::VerificationRequest(request) =
                                &event.content.msgtype
                                && request.to == own
                            {
                                *recorder.lock().unwrap() =
                                    Some((event.sender.clone(), event.event_id.to_string()));
                            }
                            std::future::ready(())
                        });
                        self.bob = Some(client);
                    }
                    "bob2" => self.bob2 = Some(client),
                    "alice" => {
                        let recorder = self.requested.clone();
                        let own = client.user_id().context("signed in")?.to_owned();
                        client.add_event_handler(
                            move |event: ToDeviceKeyVerificationRequestEvent| {
                                if event.sender == own {
                                    *recorder.lock().unwrap() =
                                        Some(event.content.transaction_id.to_string());
                                }
                                std::future::ready(())
                            },
                        );
                        self.alice = Some(client);
                    }
                    _ => unreachable!("checked above"),
                }
                Ok(reply)
            }
            "recover" => {
                let path = words.next().context("key file")?;
                if !path.contains(".local") {
                    bail!("the key file must be under .local");
                }
                let key = std::fs::read_to_string(path)?;
                let client = self.client("alice")?;
                client.encryption().recovery().recover(key.trim()).await?;
                client
                    .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
                    .await?;
                let own = client.user_id().context("signed in")?.to_owned();
                let verified = client
                    .encryption()
                    .get_user_identity(&own)
                    .await?
                    .is_some_and(|identity| identity.is_verified());
                self.known = own_devices(client).await?;
                Ok(json!({"ok": true, "identity_verified": verified}))
            }
            "wait-new-device" => {
                let seconds: u64 = words.next().context("seconds")?.parse()?;
                let client = self.client("alice")?.clone();
                let deadline = Instant::now() + Duration::from_secs(seconds);
                loop {
                    // A device-list change is fetched before the next sync.
                    let _ = client
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(300)))
                        .await;
                    let devices = own_devices(&client).await?;
                    let new: Vec<String> = devices
                        .difference(&self.known)
                        .map(ToString::to_string)
                        .collect();
                    if !new.is_empty() {
                        self.known = devices;
                        return Ok(json!({"new_devices": new}));
                    }
                    if Instant::now() >= deadline {
                        return Ok(json!({"timeout": true}));
                    }
                }
            }
            "verify-accept" | "verify-request" | "contact-accept" | "contact-request" => {
                let contact = command.starts_with("contact-");
                let target = if command.ends_with("-request") {
                    Some(words.next().context("device or user id")?.to_owned())
                } else {
                    None
                };
                let seconds: u64 = words.next().context("seconds")?.parse()?;
                let client = self.client(if contact { "bob" } else { "alice" })?.clone();
                let own = client.user_id().context("signed in")?.to_owned();
                self.sas = None;
                self.request = None;
                self.verifier = Some(client.clone());
                match target {
                    Some(target) if contact => {
                        let user: OwnedUserId = target.as_str().try_into()?;
                        // The SDK sends the request to the existing
                        // direct-message room with that user.
                        client.get_dm_room(&user).context("no direct room")?;
                        let identity = client
                            .encryption()
                            .get_user_identity(&user)
                            .await?
                            .context("no identity")?;
                        self.request = Some(identity.request_verification().await?);
                    }
                    Some(target) => {
                        let device = client
                            .encryption()
                            .get_device(&own, target.as_str().into())
                            .await?
                            .context("unknown device")?;
                        self.request = Some(device.request_verification().await?);
                    }
                    None => {}
                }
                let deadline = Instant::now() + Duration::from_secs(seconds);
                loop {
                    let _ = client
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(300)))
                        .await;
                    if self.request.is_none() {
                        let noted = if contact {
                            self.contact_requested.lock().unwrap().take()
                        } else {
                            self.requested
                                .lock()
                                .unwrap()
                                .take()
                                .map(|flow_id| (own.clone(), flow_id))
                        };
                        if let Some((sender, flow_id)) = noted {
                            self.request = client
                                .encryption()
                                .get_verification_request(&sender, &flow_id)
                                .await;
                        }
                    }
                    if let Some(request) = self.request.clone() {
                        match request.state() {
                            VerificationRequestState::Requested { .. } => {
                                request.accept().await?;
                            }
                            VerificationRequestState::Ready { .. }
                                if request.we_started() && self.sas.is_none() =>
                            {
                                self.sas = request.start_sas().await?;
                            }
                            VerificationRequestState::Transitioned { verification }
                                if self.sas.is_none() =>
                            {
                                self.sas = verification.sas();
                            }
                            VerificationRequestState::Cancelled(_) => {
                                return Ok(json!({"cancelled": true}));
                            }
                            _ => {}
                        }
                    }
                    if let Some(sas) = &self.sas {
                        match sas.state() {
                            SasState::Started { .. } if !sas.we_started() => sas.accept().await?,
                            SasState::KeysExchanged { .. } => {
                                return Ok(
                                    json!({"emoji": emoji(sas), "flow_id": request_id(&self.request)}),
                                );
                            }
                            SasState::Cancelled(_) => return Ok(json!({"cancelled": true})),
                            _ => {}
                        }
                    }
                    if Instant::now() >= deadline {
                        return Ok(json!({"timeout": true}));
                    }
                }
            }
            "verify-confirm" => {
                self.sas
                    .as_ref()
                    .context("no comparison")?
                    .confirm()
                    .await?;
                Ok(json!({"ok": true}))
            }
            "verify-mismatch" => {
                self.sas
                    .as_ref()
                    .context("no comparison")?
                    .mismatch()
                    .await?;
                Ok(json!({"ok": true}))
            }
            "verify-wait" => {
                let seconds: u64 = words.next().context("seconds")?.parse()?;
                let client = self.verifier.clone().context("no flow")?;
                let deadline = Instant::now() + Duration::from_secs(seconds);
                loop {
                    let _ = client
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(300)))
                        .await;
                    let sas = self.sas.as_ref().context("no comparison")?;
                    if sas.is_done() {
                        return Ok(json!({"done": true}));
                    }
                    if sas.is_cancelled() {
                        return Ok(json!({"cancelled": true}));
                    }
                    if Instant::now() >= deadline {
                        return Ok(json!({"timeout": true}));
                    }
                }
            }
            "bootstrap" => {
                let client = self.client("bob")?;
                let encryption = client.encryption();
                if let Err(error) = encryption.bootstrap_cross_signing(None).await {
                    let info = error
                        .as_uiaa_response()
                        .context("no authentication round")?;
                    let user = client.user_id().context("signed in")?.to_string();
                    let mut auth = Password::new(
                        UserIdentifier::Matrix(MatrixUserIdentifier::new(user)),
                        self.account.password.clone(),
                    );
                    auth.session = info.session.clone();
                    encryption
                        .bootstrap_cross_signing(Some(AuthData::Password(auth)))
                        .await?;
                }
                Ok(json!({"ok": true}))
            }
            "reset-identity" => {
                let client = self.client("bob")?;
                if let Some(handle) = client.encryption().reset_cross_signing().await? {
                    let CrossSigningResetAuthType::Uiaa(uiaa) = handle.auth_type() else {
                        bail!("not a password round");
                    };
                    let user = client.user_id().context("signed in")?.to_string();
                    let mut auth = Password::new(
                        UserIdentifier::Matrix(MatrixUserIdentifier::new(user)),
                        self.account.password.clone(),
                    );
                    auth.session = uiaa.session.clone();
                    handle.auth(Some(AuthData::Password(auth))).await?;
                }
                let mut withdrawn = false;
                if let Some(user) = words.next() {
                    let user: OwnedUserId = user.try_into()?;
                    client
                        .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
                        .await?;
                    if let Some(identity) = client.encryption().get_user_identity(&user).await?
                        && identity.has_verification_violation()
                    {
                        identity.withdraw_verification().await?;
                        withdrawn = true;
                    }
                }
                Ok(json!({"ok": true, "withdrawn": withdrawn}))
            }
            "create" => {
                let kind = words.next().context("encrypted or plain")?;
                let invitee: OwnedUserId = words.next().context("user")?.try_into()?;
                if kind == "dm" {
                    // Encrypted, trusted private, invited, and marked as
                    // direct for Bob.
                    let room = self.client("bob")?.create_dm(&invitee).await?;
                    return Ok(json!({"room_id": room.room_id().to_string()}));
                }
                let mut request = CreateRoomRequest::new();
                request.name = Some(words.collect::<Vec<_>>().join(" "));
                request.preset = Some(if kind == "public" {
                    RoomPreset::PublicChat
                } else {
                    RoomPreset::PrivateChat
                });
                request.invite = vec![invitee];
                if kind == "encrypted" {
                    request.initial_state = vec![
                        InitialStateEvent::with_empty_state_key(
                            RoomEncryptionEventContent::with_recommended_defaults(),
                        )
                        .to_raw_any(),
                    ];
                }
                let room = self.client("bob")?.create_room(request).await?;
                Ok(json!({"room_id": room.room_id().to_string()}))
            }
            "send" => {
                let which = words.next().context("which client")?;
                let room_id: OwnedRoomId = words.next().context("room")?.try_into()?;
                let text = words.collect::<Vec<_>>().join(" ");
                let client = self.client(which)?;
                let room = client.get_room(&room_id).context("room not known")?;
                let sent = room.send(RoomMessageEventContent::text_plain(text)).await?;
                Ok(json!({"event_id": sent.response.event_id.to_string()}))
            }
            "expect" => {
                let which = words.next().context("which client")?;
                let room_id: OwnedRoomId = words.next().context("room")?.try_into()?;
                let seconds: u64 = words.next().context("seconds")?.parse()?;
                let text = words.collect::<Vec<_>>().join(" ");
                let client = self.client(which)?;
                let own = client.user_id().context("signed in")?.to_owned();
                let deadline = Instant::now() + Duration::from_secs(seconds);
                let mut undecryptable = 0usize;
                loop {
                    let _ = client
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(500)))
                        .await;
                    if let Some(room) = client.get_room(&room_id) {
                        let mut options = MessagesOptions::backward();
                        options.limit = UInt::from(30u32);
                        if let Ok(messages) = room.messages(options).await {
                            undecryptable = 0;
                            for event in &messages.chunk {
                                if matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. }) {
                                    undecryptable += 1;
                                    continue;
                                }
                                let Ok(AnySyncTimelineEvent::MessageLike(
                                    AnySyncMessageLikeEvent::RoomMessage(
                                        SyncMessageLikeEvent::Original(message),
                                    ),
                                )) = event.raw().deserialize()
                                else {
                                    continue;
                                };
                                if message.sender != own && message.content.body() == text {
                                    let encrypted =
                                        matches!(event.kind, TimelineEventKind::Decrypted(_));
                                    return Ok(json!({"found": true, "encrypted": encrypted}));
                                }
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        return Ok(json!({"found": false, "undecryptable": undecryptable}));
                    }
                }
            }
            "alice-join-dm" => {
                let room_id: OwnedRoomId = words.next().context("room")?.try_into()?;
                let client = self.client("alice")?;
                let room = client.join_room_by_id(&room_id).await?;
                client
                    .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
                    .await?;
                room.set_is_direct(true).await?;
                Ok(json!({"ok": true}))
            }
            "wait-devices" => {
                let which = words.next().context("which client")?;
                let user: OwnedUserId = words.next().context("user")?.try_into()?;
                let count: usize = words.next().context("count")?.parse()?;
                let seconds: u64 = words.next().context("seconds")?.parse()?;
                let client = self.client(which)?.clone();
                let deadline = Instant::now() + Duration::from_secs(seconds);
                loop {
                    let _ = client
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(300)))
                        .await;
                    let known = client
                        .encryption()
                        .get_user_devices(&user)
                        .await?
                        .devices()
                        .count();
                    if known >= count {
                        return Ok(json!({"devices": known}));
                    }
                    if Instant::now() >= deadline {
                        return Ok(json!({"timeout": true, "devices": known}));
                    }
                }
            }
            "count" => {
                let which = words.next().context("which client")?;
                let room_id: OwnedRoomId = words.next().context("room")?.try_into()?;
                let seconds: u64 = words.next().context("seconds")?.parse()?;
                let text = words.collect::<Vec<_>>().join(" ");
                let client = self.client(which)?.clone();
                let own = client.user_id().context("signed in")?.to_owned();
                let deadline = Instant::now() + Duration::from_secs(seconds);
                while Instant::now() < deadline {
                    let _ = client
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(500)))
                        .await;
                }
                let room = client.get_room(&room_id).context("room not known")?;
                let mut options = MessagesOptions::backward();
                options.limit = UInt::from(50u32);
                let messages = room.messages(options).await?;
                let mut found = 0usize;
                let mut undecryptable = Vec::new();
                for event in &messages.chunk {
                    if let TimelineEventKind::UnableToDecrypt { utd_info, .. } = &event.kind {
                        undecryptable.push(format!("{:?}", utd_info.reason));
                        continue;
                    }
                    if let Ok(AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(
                            message,
                        )),
                    )) = event.raw().deserialize()
                        && message.sender != own
                        && message.content.body() == text
                    {
                        found += 1;
                    }
                }
                Ok(json!({"found": found, "undecryptable": undecryptable}))
            }
            "sign-devices" => {
                let client = self.client("bob")?;
                let own = client.user_id().context("signed in")?.to_owned();
                let mut signed = Vec::new();
                for device in client.encryption().get_user_devices(&own).await?.devices() {
                    if !device.is_cross_signed_by_owner() {
                        device.verify().await?;
                        signed.push(device.device_id().to_string());
                    }
                }
                Ok(json!({"signed": signed}))
            }
            "trust" => {
                let which = words.next().context("which client")?;
                let user: OwnedUserId = words.next().context("user")?.try_into()?;
                let client = self.client(which)?;
                let identity = client.encryption().get_user_identity(&user).await?;
                Ok(json!({
                    "identity": identity.is_some(),
                    "verified": identity.is_some_and(|identity| identity.is_verified())
                }))
            }
            other => bail!("unknown command {other}"),
        }
    }
}

fn request_id(request: &Option<VerificationRequest>) -> Option<String> {
    request.as_ref().map(|request| request.flow_id().to_owned())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let profile = args
        .next()
        .context("usage: p4_peer <profile.json> <server>")?;
    let server = args
        .next()
        .context("usage: p4_peer <profile.json> <server>")?;
    let entries: Vec<Entry> = serde_json::from_slice(&std::fs::read(profile)?)?;
    let entry = entries
        .into_iter()
        .find(|entry| entry.name == server)
        .context("server not in profile")?;
    let url = url::Url::parse(&entry.base_url)?;
    if !matches!(url.host_str(), Some("127.0.0.1" | "[::1]")) {
        bail!("loopback disposable servers only");
    }
    std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut peer = Peer {
                    base_url: entry.base_url,
                    account: entry.recipient,
                    first: Account {
                        username: entry.username,
                        password: entry.password,
                    },
                    alice: None,
                    requested: Arc::default(),
                    contact_requested: Arc::default(),
                    verifier: None,
                    request: None,
                    sas: None,
                    known: HashSet::new(),
                    bob: None,
                    bob2: None,
                };
                for line in std::io::stdin().lock().lines() {
                    let line = line?;
                    if line.trim() == "quit" {
                        break;
                    }
                    let reply = match Box::pin(peer.run(&line)).await {
                        Ok(value) => value,
                        // Error kinds only: no request URLs or bodies.
                        Err(error) => json!({"error": error.to_string().chars().take(120).collect::<String>()}),
                    };
                    println!("{reply}");
                }
                drop(peer.sas.take());
                drop(peer.request.take());
                drop(peer.verifier.take());
                for client in [peer.bob, peer.bob2, peer.alice].into_iter().flatten() {
                    let _ = client.matrix_auth().logout().await;
                }
                Ok(())
            })
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("driver thread panicked"))?
}
