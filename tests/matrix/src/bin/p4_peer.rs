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
//! * `create <encrypted|plain|public> <user id> <name>`: `bob` creates a
//!   room with that name and invites the user: private and encrypted,
//!   private without encryption state (some servers encrypt it anyway), or
//!   public without encryption.
//! * `send <bob|bob2> <room id> <text>`
//! * `expect <bob|bob2> <room id> <seconds> <text>`: waits for a message with
//!   exactly this text from someone else and reports whether it arrived and
//!   whether it arrived encrypted.
//! * `quit`

use anyhow::{Context, Result, bail};
use matrix_sdk::{
    Client,
    config::{RequestConfig, SyncSettings},
    deserialized_responses::TimelineEventKind,
    room::MessagesOptions,
    ruma::{
        OwnedRoomId, OwnedUserId, UInt,
        api::client::{
            room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
            uiaa::{AuthData, MatrixUserIdentifier, Password, UserIdentifier},
        },
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, InitialStateEvent, SyncMessageLikeEvent,
            room::{encryption::RoomEncryptionEventContent, message::RoomMessageEventContent},
        },
    },
};
use matrix_sdk_crypto::{CollectStrategy, DecryptionSettings, TrustRequirement};
use persephone_test_matrix::policy_http_client_builder;
use serde::Deserialize;
use serde_json::json;
use std::{
    io::BufRead,
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
    recipient: Account,
}

struct Peer {
    base_url: String,
    account: Account,
    bob: Option<Client>,
    bob2: Option<Client>,
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
            _ => None,
        }
        .context("unknown or signed-out client")
    }

    async fn sync_all(&self) {
        for client in [&self.bob, &self.bob2].into_iter().flatten() {
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
                let client = build(&self.base_url, which == "bob").await?;
                client
                    .matrix_auth()
                    .login_username(&self.account.username, &self.account.password)
                    .initial_device_display_name(if which == "bob" {
                        "P4 peer"
                    } else {
                        "P4 unsigned"
                    })
                    .await?;
                client
                    .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
                    .await?;
                let reply = json!({"ok": true, "user": client.user_id().map(|u| u.to_string())});
                match which {
                    "bob" => self.bob = Some(client),
                    "bob2" => self.bob2 = Some(client),
                    _ => bail!("login bob or bob2"),
                }
                Ok(reply)
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
            "create" => {
                let kind = words.next().context("encrypted or plain")?;
                let invitee: OwnedUserId = words.next().context("user")?.try_into()?;
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
            other => bail!("unknown command {other}"),
        }
    }
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
                for client in [peer.bob, peer.bob2].into_iter().flatten() {
                    let _ = client.matrix_auth().logout().await;
                }
                Ok(())
            })
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("driver thread panicked"))?
}
