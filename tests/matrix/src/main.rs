use anyhow::{Context, Result, ensure};
use matrix_sdk::{
    config::SyncSettings,
    room::MessagesOptions,
    ruma::{
        api::client::room::{Visibility, create_room::v3::RoomPreset},
        api::client::{keys::get_keys, room::create_room},
        presence::PresenceState,
    },
    sliding_sync::{SlidingSyncList, SlidingSyncMode, Version},
};
use persephone_test_matrix::{SyncMode, client, discover};
use serde::Deserialize;
use serde_json::json;
use std::{fs, net::IpAddr, time::Duration};

#[derive(Deserialize)]
struct Profile {
    name: String,
    version: String,
    base_url: String,
    username: String,
    password: String,
}

fn validate(profile: &Profile) -> Result<()> {
    let url = url::Url::parse(&profile.base_url)?;
    let host = url.host_str().context("missing literal loopback host")?;
    let ip: IpAddr = host.trim_matches(['[', ']']).parse()?;
    ensure!(ip.is_loopback(), "live probe requires literal loopback");
    ensure!(
        url.scheme() == "http",
        "only disposable HTTP loopback is supported"
    );
    ensure!(
        url.port().is_some()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "expected bare disposable base URL"
    );
    ensure!(
        profile.username == "p0_alice",
        "only harness synthetic user allowed"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let profile_path = args
        .next()
        .context("usage: probe profile.json results.json")?;
    let output = args.next().context("missing results path")?;
    let profiles: Vec<Profile> = serde_json::from_slice(&fs::read(profile_path)?)?;
    ensure!(!profiles.is_empty(), "empty profiles");
    for profile in &profiles {
        validate(profile)?;
    }
    let mut results = Vec::new();
    for profile in profiles {
        let c = client(&profile.base_url).await?;
        ensure!(
            discover(&c).await? == SyncMode::Sliding,
            "required native capability absent on {}",
            profile.name
        );
        c.matrix_auth()
            .login_username(&profile.username, &profile.password)
            .initial_device_display_name("Disposable Persephone P0 probe")
            .send()
            .await
            .context("synthetic login failed")?;
        let original_user = c.user_id().context("missing user")?.to_owned();
        let original_device = c.device_id().context("missing device")?.to_owned();
        let original_keys = c
            .encryption()
            .get_own_device()
            .await?
            .context("missing own crypto device")?
            .keys()
            .clone();

        // Public synthetic room avoids representing this protocol/concurrency
        // smoke test as an encrypted cross-client messaging proof (P4).
        let mut create = create_room::v3::Request::new();
        create.visibility = Visibility::Public;
        create.preset = Some(RoomPreset::PublicChat);
        create.name = Some("Disposable P0 protocol probe".into());
        let room = c.create_room(create).await?;
        let sliding = c
            .sliding_sync("p0-probe")?
            .version(Version::Native)
            .add_list(
                SlidingSyncList::builder("rooms")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=9))
                    .timeline_limit(10),
            )
            .with_all_extensions()
            .without_typing_extension()
            .without_receipt_extension()
            .poll_timeout(Duration::from_millis(100))
            .network_timeout(Duration::from_secs(5))
            .build()
            .await?;
        // Do not cancel a returned-response processing task. Await successful
        // completion before switching; forced cancellation remains a P0 gate.
        sliding
            .sync_once()
            .await
            .context("initial native sliding sync")?;
        let history = room.messages(MessagesOptions::backward());
        let mut key_query = get_keys::v3::Request::new();
        key_query.device_keys.insert(original_user.clone(), vec![]);
        let (sync, history, keys) = tokio::join!(
            sliding.sync_once(),
            history,
            c.send(key_query).into_future()
        );
        sync.context("concurrent sliding sync")?;
        let history_events = history
            .context("concurrent history pagination")?
            .chunk
            .len();
        keys.context("concurrent key query")?;

        // All three operations are complete. No independent encryption sync
        // service is started by this low-level investigation.
        drop(sliding);
        c.sync_once(
            SyncSettings::default()
                .timeout(Duration::from_millis(100))
                .set_presence(PresenceState::Offline),
        )
        .await
        .context("classic after sliding")?;
        ensure!(
            c.user_id() == Some(original_user.as_ref())
                && c.device_id() == Some(original_device.as_ref()),
            "identity changed"
        );
        let keys_after = c
            .encryption()
            .get_own_device()
            .await?
            .context("crypto device missing after mode switch")?
            .keys()
            .clone();
        ensure!(original_keys == keys_after, "own device keys changed");
        ensure!(c.get_room(room.room_id()).is_some(), "room projection lost");
        results.push(json!({
            "server": profile.name, "reported_version": profile.version,
            "sdk": "0.19.1", "native_sliding": "passed", "classic_after_sliding": "passed",
            "concurrent_history_and_key_query": "passed", "history_events": history_events,
            "account_device_and_own_keys_preserved": true,
            "limits": ["in-memory SDK store", "completed operations before switch", "no forced-cancellation/drain proof", "not encrypted-message interoperability"]
        }));
        println!(
            "PASS {}: Sliding, concurrent history/key query, classic, identity/key continuity",
            profile.name
        );
    }
    fs::write(output, serde_json::to_vec_pretty(&results)?)?;
    Ok(())
}

use std::future::IntoFuture;
