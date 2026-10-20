use anyhow::{Context, Result, anyhow, ensure};
use matrix_sdk::{
    Client,
    config::{RequestConfig, SyncSettings},
    room::MessagesOptions,
    ruma::{
        api::client::message::get_message_events::v3::Request as GetMessagesRequest,
        events::{InitialStateEvent, room::encryption::RoomEncryptionEventContent},
    },
};
use serde::Deserialize;
use std::{net::IpAddr, path::PathBuf, time::Duration};
use url::Url;

const USERNAME: &str = "p0_alice";
const SMOKE_TEXT: &str = "persephone-p0-e2ee-smoke-fixed-synthetic-text";
const STEP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
struct Profile {
    name: String,
    version: String,
    base_url: String,
    username: String,
    password: String,
}

fn profile_path() -> Result<PathBuf> {
    std::env::var_os("PERSEPHONE_P0_PROFILE")
        .map(PathBuf::from)
        .context("set PERSEPHONE_P0_PROFILE to the current disposable runner's profile")
}

fn read_profiles() -> Result<Vec<Profile>> {
    let path = profile_path()?;
    let content = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "could not read disposable fixture profile at {}",
            path.display()
        )
    })?;
    let profiles: Vec<Profile> = serde_json::from_str(&content)
        .context("disposable fixture profile is not a valid profile list")?;
    for profile in &profiles {
        validate_profile(profile)?;
    }
    Ok(profiles)
}

fn validate_profile(profile: &Profile) -> Result<()> {
    let url = Url::parse(&profile.base_url).context("fixture endpoint URL is invalid")?;
    ensure!(
        url.scheme() == "http",
        "fixture endpoint must use literal loopback HTTP"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "fixture URL must not contain user information"
    );
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "fixture URL must be a bare origin"
    );
    ensure!(url.port().is_some(), "fixture endpoint must specify a port");
    match url.host().context("fixture endpoint has no host")? {
        url::Host::Ipv4(ip) => ensure!(
            IpAddr::V4(ip).is_loopback(),
            "fixture endpoint must use literal loopback"
        ),
        url::Host::Ipv6(ip) => ensure!(
            IpAddr::V6(ip).is_loopback(),
            "fixture endpoint must use literal loopback"
        ),
        url::Host::Domain(_) => {
            return Err(anyhow!(
                "fixture endpoint host must be a literal loopback address"
            ));
        }
    }
    ensure!(
        profile.username == USERNAME,
        "fixture account must be p0_alice"
    );
    ensure!(!profile.password.is_empty(), "fixture password is empty");
    Ok(())
}

async fn make_client(profile: &Profile) -> Result<Client> {
    Client::builder()
        .homeserver_url(&profile.base_url)
        .http_client(
            persephone_test_matrix::policy_http_client_builder()
                .build()
                .context("could not build fixture-only HTTP client")?,
        )
        .request_config(
            RequestConfig::new()
                .disable_retry()
                .timeout(Duration::from_secs(3)),
        )
        .build()
        .await
        .context("could not initialize Matrix SDK client")
}

async fn bounded<T>(
    stage: &'static str,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .map_err(|_| anyhow!("bounded step timed out: {stage}"))?
        .map_err(|_| anyhow!("step failed: {stage}"))
}

async fn one_sync(client: &Client, stage: &'static str) -> Result<()> {
    bounded(stage, async {
        client
            .sync_once(SyncSettings::default().timeout(Duration::from_millis(150)))
            .await
            .context("Matrix SDK sync request failed")?;
        Ok(())
    })
    .await
}

async fn exercise(profile: &Profile) -> Result<()> {
    let client1 = make_client(profile).await?;
    bounded("first device login", async {
        client1
            .matrix_auth()
            .login_username(USERNAME, &profile.password)
            .device_id(&format!("P0E1{}", std::process::id()))
            .initial_device_display_name("P0 synthetic encrypted smoke device 1")
            .send()
            .await
            .context("Matrix SDK login failed")?;
        Ok(())
    })
    .await?;
    one_sync(&client1, "first device initial key processing").await?;

    let client2 = make_client(profile).await?;
    bounded("second device login", async {
        client2
            .matrix_auth()
            .login_username(USERNAME, &profile.password)
            .device_id(&format!("P0E2{}", std::process::id()))
            .initial_device_display_name("P0 synthetic encrypted smoke device 2")
            .send()
            .await
            .context("Matrix SDK login failed")?;
        Ok(())
    })
    .await?;
    one_sync(&client2, "second device initial key processing").await?;
    one_sync(&client1, "sender device-list and key processing").await?;

    let encryption_state = InitialStateEvent::with_empty_state_key(
        RoomEncryptionEventContent::with_recommended_defaults(),
    )
    .to_raw_any();
    let mut room_request = matrix_sdk::ruma::api::client::room::create_room::v3::Request::new();
    room_request.name = Some("P0 synthetic E2EE smoke".to_owned());
    room_request.preset =
        Some(matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset::PrivateChat);
    room_request.initial_state = vec![encryption_state];
    let sender_room = bounded("create private encrypted room", async {
        client1
            .create_room(room_request)
            .await
            .context("Matrix SDK room creation failed")
    })
    .await?;
    one_sync(&client2, "recipient device room and key processing").await?;

    let event_id = bounded("send encrypted synthetic message", async {
        sender_room
            .send(
                matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
                    SMOKE_TEXT,
                ),
            )
            .await
            .context("Matrix SDK encrypted send failed")
    })
    .await?
    .response
    .event_id;

    one_sync(
        &client2,
        "recipient encrypted-event and to-device key processing",
    )
    .await?;
    let recipient_room = client2
        .get_room(sender_room.room_id())
        .context("second device did not join the same-account encrypted room")?;
    let raw_history = bounded("read raw wire history", async {
        client2
            .send(GetMessagesRequest::backward(
                sender_room.room_id().to_owned(),
            ))
            .await
            .context("raw history request failed")
    })
    .await?;
    let encrypted_wire_event = raw_history.chunk.iter().any(|event| {
        event.get_field::<String>("type").ok().flatten().as_deref() == Some("m.room.encrypted")
            && event
                .get_field::<String>("event_id")
                .ok()
                .flatten()
                .as_deref()
                == Some(event_id.as_str())
    });
    ensure!(
        encrypted_wire_event,
        "server history did not contain the sent event as m.room.encrypted"
    );

    let decrypted_history = bounded("SDK-decrypted room history", async {
        recipient_room
            .messages(MessagesOptions::backward())
            .await
            .context("Matrix SDK history/decryption request failed")
    })
    .await?;
    let decrypted = decrypted_history.chunk.iter().any(|event| {
        let json = serde_json::from_str::<serde_json::Value>(event.raw().json().get()).ok();
        json.and_then(|value| {
            let event_type = value.get("type")?.as_str()?;
            let returned_event_id = value.get("event_id")?.as_str()?;
            let body = value.get("content")?.get("body")?.as_str()?;
            Some(
                event_type == "m.room.message"
                    && returned_event_id == event_id.as_str()
                    && body == SMOKE_TEXT,
            )
        })
        .unwrap_or(false)
    });
    ensure!(
        decrypted,
        "second device did not decrypt the expected synthetic message"
    );

    let logout1 = tokio::time::timeout(STEP_TIMEOUT, client1.matrix_auth().logout()).await;
    let logout2 = tokio::time::timeout(STEP_TIMEOUT, client2.matrix_auth().logout()).await;
    ensure!(
        logout1.is_ok_and(|result| result.is_ok()),
        "first device logout was not confirmed"
    );
    ensure!(
        logout2.is_ok_and(|result| result.is_ok()),
        "second device logout was not confirmed"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let profiles = read_profiles()?;
    ensure!(!profiles.is_empty(), "fixture profile list is empty");
    let mut failed = false;
    for profile in &profiles {
        println!(
            "{} {}: starting bounded same-account two-device E2EE smoke",
            profile.name, profile.version
        );
        match exercise(profile).await {
            Ok(()) => println!(
                "{}: PASS raw m.room.encrypted event, matching SDK-decrypted message, and both logouts",
                profile.name
            ),
            Err(error) => {
                eprintln!("{}: FAIL {error}", profile.name);
                failed = true;
            }
        }
    }
    ensure!(!failed, "one or more disposable homeserver smokes failed");
    Ok(())
}
