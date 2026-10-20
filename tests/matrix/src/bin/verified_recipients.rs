//! Disposable two-user SAS verification and bidirectional encrypted messaging.
//! The trusted test controller simulates the users' out-of-band SAS comparison.
//! This is not a production verification UI or a cross-signing/recovery test.
use anyhow::{Context, Result, anyhow, ensure};
use matrix_sdk::{
    Client,
    encryption::{identities::Device, verification::VerificationRequest},
    room::{MessagesOptions, Room},
    ruma::{
        OwnedEventId,
        api::client::{
            message::get_message_events::v3::Request as GetMessagesRequest,
            room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        },
        events::{
            InitialStateEvent,
            key::verification::VerificationMethod,
            room::{encryption::RoomEncryptionEventContent, message::RoomMessageEventContent},
        },
    },
};
use persephone_test_matrix::{
    sync_fixture::{ScenarioMode, SyncPair},
    sync_receipts::SyncReceipts,
};
use serde::{Deserialize, Serialize};
use std::{future::Future, path::Path, time::Duration};

#[path = "../capability_fixture.rs"]
mod capability_fixture;

#[path = "../restart_fixture.rs"]
mod restart_fixture;

const STEP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
// Debug SDK SAS futures exceed this executable's default 1 MiB Windows stack.
// Bound the synthetic driver to one explicitly sized thread; this is not an
// application stack/memory-budget result or a change to the worker sandbox.
const DRIVER_STACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct Account {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct Profile {
    name: String,
    version: String,
    base_url: String,
    username: String,
    password: String,
    recipient: Account,
}

fn validate(profile: &Profile) -> Result<()> {
    let url = url::Url::parse(&profile.base_url).context("invalid fixture origin")?;
    ensure!(
        url.scheme() == "http"
            && url.port().is_some()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "expected a bare loopback HTTP fixture origin with an explicit port"
    );
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    ensure!(loopback, "fixture origin must use a literal loopback IP");
    ensure!(
        matches!(profile.name.as_str(), "merovingian" | "synapse"),
        "unknown disposable server name"
    );
    ensure!(
        profile.username == "p0_alice" && profile.recipient.username == "p0_bob",
        "only the two distinct harness accounts are allowed"
    );
    ensure!(
        !profile.password.is_empty()
            && !profile.recipient.password.is_empty()
            && profile.password != profile.recipient.password,
        "fixture accounts require distinct nonempty passwords"
    );
    Ok(())
}

async fn bounded<T>(stage: &'static str, future: impl Future<Output = Result<T>>) -> Result<T> {
    // SDK errors can contain request details. Keep failure output stage-only.
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .map_err(|_| anyhow!("step timed out: {stage}"))?
        .map_err(|error| {
            let reason = if let Some(error) = error.downcast_ref::<matrix_sdk::Error>() {
                if let Some(api) = error.as_client_api_error() {
                    format!("Matrix HTTP {}", api.status_code.as_u16())
                } else if let matrix_sdk::Error::Http(http) = error {
                    match http.as_ref() {
                        matrix_sdk::HttpError::Reqwest(error) if error.is_timeout() => {
                            "HTTP timeout".into()
                        }
                        matrix_sdk::HttpError::Reqwest(error) if error.is_connect() => {
                            "HTTP connect failure".into()
                        }
                        _ => "SDK HTTP failure".into(),
                    }
                } else {
                    "SDK failure".into()
                }
            } else {
                "fixture assertion or initialization failure".into()
            };
            anyhow!("step failed: {stage} ({reason})")
        })
}

async fn make_client(profile: &Profile) -> Result<Client> {
    bounded("initialize fixture SDK client", async {
        Ok(
            persephone_test_matrix::fixture_client_builder(&profile.base_url)?
                .build()
                .await?,
        )
    })
    .await
}

async fn sync_pair(syncs: &SyncPair<'_>) -> Result<()> {
    bounded(
        "selected sync engines and SDK key processing",
        syncs.cycle(),
    )
    .await
}

async fn wait_for<T, F, Fut>(
    syncs: &SyncPair<'_>,
    stage: &'static str,
    mut condition: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    tokio::time::timeout(POLL_TIMEOUT, async {
        loop {
            if let Some(value) = condition().await? {
                return Ok(value);
            }
            sync_pair(syncs).await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("verification progress timed out: {stage}"))?
}

async fn peer_device(observer: &Client, peer: &Client) -> Result<Device> {
    bounded("load expected peer device", async {
        observer
            .encryption()
            .get_device(
                peer.user_id().context("peer is not logged in")?,
                peer.device_id().context("peer has no device ID")?,
            )
            .await?
            .context("expected peer device is missing")
    })
    .await
}

async fn assert_trust(alice: &Client, bob: &Client, expected: bool) -> Result<()> {
    ensure!(
        peer_device(alice, bob).await?.is_verified() == expected
            && peer_device(bob, alice).await?.is_verified() == expected,
        "mutual device verification state differs from the expected state"
    );
    Ok(())
}

async fn request_verification(alice: &Client, bob: &Client) -> Result<VerificationRequest> {
    bounded("request peer SAS verification", async {
        Ok(peer_device(alice, bob)
            .await?
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await?)
    })
    .await
}

async fn receive_request(
    syncs: &SyncPair<'_>,
    alice: &Client,
    bob: &Client,
    flow_id: &str,
) -> Result<VerificationRequest> {
    wait_for(syncs, "receive peer verification request", || async {
        Ok(bob
            .encryption()
            .get_verification_request(alice.user_id().context("Alice not logged in")?, flow_id)
            .await)
    })
    .await
}

fn matching_sas(left: Option<(u16, u16, u16)>, right: Option<(u16, u16, u16)>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left == right)
}

async fn verify_devices(syncs: &SyncPair<'_>, alice: &Client, bob: &Client) -> Result<()> {
    assert_trust(alice, bob, false).await?;

    // A cancelled request must leave both peers unverified.
    let cancelled = request_verification(alice, bob).await?;
    let incoming = receive_request(syncs, alice, bob, cancelled.flow_id()).await?;
    bounded("cancel negative-control verification", async {
        Ok(incoming.cancel().await?)
    })
    .await?;
    wait_for(syncs, "receive verification cancellation", || async {
        Ok(cancelled.is_cancelled().then_some(()))
    })
    .await?;
    assert_trust(alice, bob, false).await?;

    let request = request_verification(alice, bob).await?;
    ensure!(
        request.flow_id() != cancelled.flow_id(),
        "verification flow ID was reused"
    );
    let incoming = receive_request(syncs, alice, bob, request.flow_id()).await?;
    ensure!(
        !request.is_self_verification() && !incoming.is_self_verification(),
        "expected verification between independent users"
    );
    bounded("accept peer verification request", async {
        Ok(incoming
            .accept_with_methods(vec![VerificationMethod::SasV1])
            .await?)
    })
    .await?;
    wait_for(syncs, "verification request ready", || async {
        ensure!(
            !request.is_cancelled(),
            "verification request was cancelled"
        );
        Ok(request.is_ready().then_some(()))
    })
    .await?;
    let alice_sas = bounded("start SAS", async {
        request
            .start_sas()
            .await?
            .context("request did not start SAS")
    })
    .await?;
    let bob_sas = wait_for(syncs, "receive SAS start", || async {
        Ok(bob
            .encryption()
            .get_verification(
                alice.user_id().context("Alice not logged in")?,
                request.flow_id(),
            )
            .await
            .and_then(|verification| verification.sas()))
    })
    .await?;
    for (sas, own, peer) in [(&alice_sas, alice, bob), (&bob_sas, bob, alice)] {
        ensure!(
            !sas.is_self_verification()
                && Some(sas.own_user_id()) == own.user_id()
                && Some(sas.other_user_id()) == peer.user_id()
                && Some(sas.other_device().user_id()) == peer.user_id()
                && Some(sas.other_device().device_id()) == peer.device_id(),
            "SAS flow does not identify the expected independent peer device"
        );
    }
    bounded("accept SAS", async { Ok(bob_sas.accept().await?) }).await?;
    wait_for(syncs, "exchange SAS keys", || async {
        ensure!(
            !alice_sas.is_cancelled() && !bob_sas.is_cancelled(),
            "SAS exchange was cancelled"
        );
        Ok((alice_sas.can_be_presented() && bob_sas.can_be_presented()).then_some(()))
    })
    .await?;
    assert_trust(alice, bob, false).await?;
    // Only this trusted fixture controller compares the strings directly.
    // A product must obtain an explicit user comparison over a trusted channel.
    if !matching_sas(alice_sas.decimals(), bob_sas.decimals()) {
        let _ = alice_sas.mismatch().await;
        let _ = bob_sas.mismatch().await;
        return Err(anyhow!("out-of-band SAS comparison failed"));
    }
    bounded("Alice confirms matching SAS", async {
        Ok(alice_sas.confirm().await?)
    })
    .await?;
    bounded("Bob confirms matching SAS", async {
        Ok(bob_sas.confirm().await?)
    })
    .await?;
    wait_for(syncs, "complete both authenticated SAS flows", || async {
        ensure!(
            !alice_sas.is_cancelled() && !bob_sas.is_cancelled(),
            "SAS confirmation was cancelled"
        );
        Ok(
            (alice_sas.is_done() && bob_sas.is_done() && request.is_done() && incoming.is_done())
                .then_some(()),
        )
    })
    .await?;
    assert_trust(alice, bob, true).await
}

async fn encrypted_message(
    syncs: &SyncPair<'_>,
    receipts: &SyncReceipts,
    sender: &Client,
    recipient: &Client,
    sender_room: &Room,
    recipient_room: &Room,
    text: &'static str,
) -> Result<OwnedEventId> {
    assert_trust(sender, recipient, true).await?;
    let event_id = bounded("send encrypted message to verified peer", async {
        Ok(sender_room
            .send(RoomMessageEventContent::text_plain(text))
            .await?
            .response
            .event_id)
    })
    .await?;
    let expected_sender = sender.user_id().context("sender not logged in")?.as_str();
    // Observe decrypted sync delivery before fetching history: /messages alone
    // cannot satisfy this assertion or hide a broken native sync timeline.
    wait_for(
        syncs,
        "receive exact decrypted event through sync",
        || async {
            Ok(receipts
                .has_exact(
                    event_id.as_str(),
                    expected_sender,
                    sender_room.room_id().as_str(),
                    text,
                )?
                .then_some(()))
        },
    )
    .await?;
    let history = bounded("read exact encrypted wire event", async {
        Ok(recipient
            .send(GetMessagesRequest::backward(
                sender_room.room_id().to_owned(),
            ))
            .await?)
    })
    .await?;
    let encrypted = history.chunk.iter().any(|event| {
        serde_json::from_str::<serde_json::Value>(event.json().get()).is_ok_and(|event| {
            event["event_id"] == event_id.as_str()
                && event["sender"] == expected_sender
                && event["type"] == "m.room.encrypted"
                && event["content"]["algorithm"] == "m.megolm.v1.aes-sha2"
                && event["content"].get("body").is_none()
        })
    });
    ensure!(
        encrypted,
        "exact sent event is missing from encrypted wire history"
    );
    assert_decrypted_history(recipient_room, sender, &event_id, text).await?;
    Ok(event_id)
}

async fn assert_decrypted_history(
    recipient_room: &Room,
    sender: &Client,
    event_id: &matrix_sdk::ruma::EventId,
    text: &str,
) -> Result<()> {
    let expected_sender = sender.user_id().context("sender not logged in")?.as_str();
    let history = bounded("decrypt verified peer message", async {
        Ok(recipient_room.messages(MessagesOptions::backward()).await?)
    })
    .await?;
    let decrypted = history.chunk.iter().any(|event| {
        serde_json::from_str::<serde_json::Value>(event.raw().json().get()).is_ok_and(|event| {
            event["event_id"] == event_id.as_str()
                && event["sender"] == expected_sender
                && event["type"] == "m.room.message"
                && event["content"]["body"] == text
        })
    });
    ensure!(
        decrypted,
        "peer did not decrypt the exact event, sender and message body"
    );
    Ok(())
}

async fn security_snapshot(own: &Client, peer: &Client) -> Result<serde_json::Value> {
    let device = bounded("snapshot own crypto identity", async {
        own.encryption()
            .get_own_device()
            .await?
            .context("own device missing")
    })
    .await?;
    let peer = peer_device(own, peer).await?;
    let own_keys: std::collections::BTreeMap<_, _> = device
        .keys()
        .iter()
        .map(|(id, key)| (id.as_str().to_owned(), key.to_base64()))
        .collect();
    let peer_keys: std::collections::BTreeMap<_, _> = peer
        .keys()
        .iter()
        .map(|(id, key)| (id.as_str().to_owned(), key.to_base64()))
        .collect();
    // Public device keys stay in private test memory; never include the values
    // in the published results. Equality checks detect identity/key changes.
    Ok(serde_json::json!({
        "user": own.user_id(), "device": own.device_id(), "keys": own_keys,
        "peer_user": peer.user_id(), "peer_device": peer.device_id(),
        "peer_keys": peer_keys, "peer_verified": peer.is_verified(),
    }))
}

async fn login_pair(profile: &Profile, alice: &Client, bob: &Client) -> Result<()> {
    for (client, username, password, suffix) in [
        (alice, &profile.username, &profile.password, "ALICE"),
        (
            bob,
            &profile.recipient.username,
            &profile.recipient.password,
            "BOB",
        ),
    ] {
        bounded("independent account login", async {
            client
                .matrix_auth()
                .login_username(username, password)
                .device_id(&format!("VERIFIED{suffix}{}", std::process::id()))
                .initial_device_display_name("Persephone disposable recipient verification")
                .send()
                .await?;
            Ok(())
        })
        .await?;
        ensure!(
            client
                .user_id()
                .is_some_and(|id| id.localpart() == username),
            "unexpected login identity"
        );
    }
    ensure!(
        alice.user_id() != bob.user_id(),
        "fixture returned the same user for both logins"
    );
    Ok(())
}

async fn create_verified_room(
    profile: &Profile,
    alice: &Client,
    bob: &Client,
    syncs: &SyncPair<'_>,
) -> Result<(Room, Room)> {
    sync_pair(syncs).await?;
    let mut request = CreateRoomRequest::new();
    request.name = Some("Disposable verified recipient test".to_owned());
    request.preset = Some(RoomPreset::PrivateChat);
    request.invite = vec![bob.user_id().context("Bob not logged in")?.to_owned()];
    request.initial_state = vec![
        InitialStateEvent::with_empty_state_key(
            RoomEncryptionEventContent::with_recommended_defaults(),
        )
        .to_raw_any(),
    ];
    let alice_room = bounded("create private encrypted room and invite Bob", async {
        Ok(alice.create_room(request).await?)
    })
    .await?;
    syncs.subscribe(alice_room.room_id());
    sync_pair(syncs).await?;
    let bob_room = bounded("independent recipient joins room", async {
        Ok(bob.join_room_by_id(alice_room.room_id()).await?)
    })
    .await?;
    sync_pair(syncs).await?;
    sync_pair(syncs).await?;
    println!(
        "{}: independent users joined the encrypted room",
        profile.name
    );
    // Avoid retaining the nested verification future inside the caller's state.
    Box::pin(verify_devices(syncs, alice, bob)).await?;
    println!(
        "{}: cancelled flow stayed untrusted; mutual SAS verification completed",
        profile.name
    );
    Ok((alice_room, bob_room))
}

async fn scenario(
    profile: &Profile,
    alice: &Client,
    bob: &Client,
    mode: ScenarioMode,
) -> Result<Option<SlidingChecks>> {
    login_pair(profile, alice, bob).await?;
    let mut syncs = bounded(
        "initialize selected sync engines",
        SyncPair::new(alice, bob, mode),
    )
    .await?;
    let alice_receipts = SyncReceipts::new(alice);
    let bob_receipts = SyncReceipts::new(bob);
    let (alice_room, bob_room) =
        Box::pin(create_verified_room(profile, alice, bob, &syncs)).await?;
    let alice_event = encrypted_message(
        &syncs,
        &bob_receipts,
        alice,
        bob,
        &alice_room,
        &bob_room,
        "synthetic verified Alice to Bob",
    )
    .await?;
    let bob_event = encrypted_message(
        &syncs,
        &alice_receipts,
        bob,
        alice,
        &bob_room,
        &alice_room,
        "synthetic verified Bob to Alice",
    )
    .await?;
    assert_trust(alice, bob, true).await?;
    if mode == ScenarioMode::ClassicOnly {
        return Ok(None);
    }
    let (native_rounds, classic_rounds_before) = syncs.completed_rounds();
    ensure!(
        native_rounds > 0 && classic_rounds_before == 0,
        "native stage used classic sync"
    );
    let alice_identity = security_snapshot(alice, bob).await?;
    let bob_identity = security_snapshot(bob, alice).await?;
    // Every selected sync call, callback and verification step above completed.
    // Failure or an outer timeout exits the scenario before this boundary.
    syncs.transition_to_classic()?;
    sync_pair(&syncs).await?;
    ensure!(
        alice_identity == security_snapshot(alice, bob).await?
            && bob_identity == security_snapshot(bob, alice).await?,
        "identity, device keys or peer verification changed across sync modes"
    );
    assert_trust(alice, bob, true).await?;
    assert_decrypted_history(
        &bob_room,
        alice,
        &alice_event,
        "synthetic verified Alice to Bob",
    )
    .await?;
    assert_decrypted_history(
        &alice_room,
        bob,
        &bob_event,
        "synthetic verified Bob to Alice",
    )
    .await?;
    encrypted_message(
        &syncs,
        &bob_receipts,
        alice,
        bob,
        &alice_room,
        &bob_room,
        "synthetic classic-after-sliding Alice to Bob",
    )
    .await?;
    encrypted_message(
        &syncs,
        &alice_receipts,
        bob,
        alice,
        &bob_room,
        &alice_room,
        "synthetic classic-after-sliding Bob to Alice",
    )
    .await?;
    ensure!(
        alice_identity == security_snapshot(alice, bob).await?
            && bob_identity == security_snapshot(bob, alice).await?,
        "identity, device keys or peer verification changed after classic messages"
    );
    let (_, classic_rounds) = syncs.completed_rounds();
    ensure!(classic_rounds > 0, "no classic sync completed");
    println!(
        "{}: native encrypted delivery and completed-operation classic transition passed",
        profile.name
    );
    Ok(Some(SlidingChecks {
        native_rounds_completed: native_rounds,
        classic_driver_rounds_before_transition: classic_rounds_before,
        sas_verified_using_native_sync: true,
        native_bidirectional_decrypted_sync_receipts: true,
        completed_operation_transition: true,
        both_identities_keys_and_peer_verification_preserved: true,
        native_messages_decrypt_after_classic_transition: true,
        classic_bidirectional_decrypted_sync_receipts: true,
        classic_rounds_completed: classic_rounds,
    }))
}

async fn logout(client: &Client) -> bool {
    if client.user_id().is_none() {
        return false;
    }
    bounded("fixture logout", async {
        Ok(client.matrix_auth().logout().await?)
    })
    .await
    .is_ok()
}

#[derive(Serialize)]
struct ServerResult {
    server: String,
    version: String,
    passed: bool,
    mode: ScenarioMode,
    checks: Option<Checks>,
    sliding_sync: Option<SlidingChecks>,
    both_logouts_confirmed: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct Checks {
    distinct_user_ids: bool,
    initially_unverified: bool,
    cancelled_request_did_not_verify: bool,
    sas_bound_to_expected_users_and_devices: bool,
    out_of_band_sas_matched: bool,
    both_sas_flows_done: bool,
    mutually_verified_before_sending: bool,
    alice_to_bob_exact_encrypted_and_decrypted_event: bool,
    bob_to_alice_exact_encrypted_and_decrypted_event: bool,
    sync_handler_decrypted_both_directions: bool,
}

#[derive(Clone, Copy, Serialize)]
struct SlidingChecks {
    native_rounds_completed: u32,
    classic_driver_rounds_before_transition: u32,
    sas_verified_using_native_sync: bool,
    native_bidirectional_decrypted_sync_receipts: bool,
    completed_operation_transition: bool,
    both_identities_keys_and_peer_verification_preserved: bool,
    native_messages_decrypt_after_classic_transition: bool,
    classic_bidirectional_decrypted_sync_receipts: bool,
    classic_rounds_completed: u32,
}

async fn exercise(profile: &Profile, mode: ScenarioMode) -> Result<ServerResult> {
    let alice = make_client(profile).await?;
    let bob = make_client(profile).await?;
    let result = Box::pin(scenario(profile, &alice, &bob, mode)).await;
    // Try both logouts even when the scenario or the first logout fails.
    let alice_logout = logout(&alice).await;
    let bob_logout = logout(&bob).await;
    let both_logouts_confirmed = alice_logout && bob_logout;
    Ok(ServerResult {
        server: profile.name.clone(),
        version: profile.version.clone(),
        passed: result.is_ok() && both_logouts_confirmed,
        mode,
        checks: result.as_ref().ok().map(|_| Checks {
            distinct_user_ids: true,
            initially_unverified: true,
            cancelled_request_did_not_verify: true,
            sas_bound_to_expected_users_and_devices: true,
            out_of_band_sas_matched: true,
            both_sas_flows_done: true,
            mutually_verified_before_sending: true,
            alice_to_bob_exact_encrypted_and_decrypted_event: true,
            bob_to_alice_exact_encrypted_and_decrypted_event: true,
            sync_handler_decrypted_both_directions: true,
        }),
        sliding_sync: result.as_ref().ok().copied().flatten(),
        both_logouts_confirmed,
        error: result.err().map(|error| error.to_string()),
    })
}

async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if Box::pin(capability_fixture::try_run(&args)).await? {
        return Ok(());
    }
    if Box::pin(restart_fixture::try_run(&args)).await? {
        return Ok(());
    }
    ensure!(
        args.len() == 2 || (args.len() == 3 && args[2] == "--sliding-sync"),
        "usage: verified_recipients <disposable-profile.json> <ignored-report.json> [--sliding-sync]"
    );
    let mode = if args.len() == 3 {
        ScenarioMode::SlidingThenClassic
    } else {
        ScenarioMode::ClassicOnly
    };
    let profiles: Vec<Profile> = serde_json::from_slice(&std::fs::read(&args[0])?)
        .context("profile must contain both independent fixture accounts")?;
    ensure!(!profiles.is_empty(), "empty fixture list");
    for profile in &profiles {
        validate(profile)?;
    }
    let output = Path::new(&args[1]);
    let local = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.local")
        .canonicalize()?;
    let parent = output
        .parent()
        .context("report needs a parent directory")?
        .canonicalize()?;
    ensure!(
        parent.starts_with(local),
        "report must stay in the ignored repository .local tree"
    );
    ensure!(!output.exists(), "refusing to overwrite an existing result");
    let mut results = Vec::new();
    for profile in &profiles {
        let result = match Box::pin(exercise(profile, mode)).await {
            Ok(result) => result,
            Err(error) => ServerResult {
                server: profile.name.clone(),
                version: profile.version.clone(),
                passed: false,
                mode,
                checks: None,
                sliding_sync: None,
                both_logouts_confirmed: false,
                error: Some(error.to_string()),
            },
        };
        println!(
            "{}: {} verified independent recipients, bidirectional E2EE, and both logouts{}",
            result.server,
            if result.passed { "PASS" } else { "FAIL" },
            result
                .error
                .as_ref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        );
        results.push(result);
    }
    std::fs::write(output, serde_json::to_vec_pretty(&results)?)?;
    ensure!(
        results.iter().all(|r| r.passed),
        "one or more verified-recipient fixtures failed"
    );
    Ok(())
}

fn main() -> Result<()> {
    std::thread::Builder::new()
        .name("verified-recipient-fixture".into())
        .stack_size(DRIVER_STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(Box::pin(run()))
        })?
        .join()
        .map_err(|_| anyhow!("verified-recipient fixture thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_missing_or_mismatched_sas_when_compared_then_confirmation_is_refused() {
        let code = Some((1234, 2345, 3456));
        assert!(!matching_sas(None, None));
        assert!(!matching_sas(code, None));
        assert!(!matching_sas(None, code));
        assert!(!matching_sas(code, Some((1234, 2345, 3457))));
        assert!(matching_sas(code, code));
    }

    #[test]
    fn given_non_disposable_or_same_account_profile_then_reject_before_login() {
        let mut profile = Profile {
            name: "merovingian".into(),
            version: "test".into(),
            base_url: "http://127.0.0.1:8080".into(),
            username: "p0_alice".into(),
            password: "synthetic-a".into(),
            recipient: Account {
                username: "p0_bob".into(),
                password: "synthetic-b".into(),
            },
        };
        assert!(validate(&profile).is_ok());
        for invalid in [
            "https://production.example:8448",
            "http://localhost:8080",
            "http://192.0.2.1:8080",
            "http://127.0.0.1:8080/path",
            "http://secret@127.0.0.1:8080",
        ] {
            profile.base_url = invalid.into();
            assert!(validate(&profile).is_err());
        }
        profile.base_url = "http://127.0.0.1:8080".into();
        profile.recipient.username = "p0_alice".into();
        assert!(validate(&profile).is_err());
        profile.recipient.username = "p0_bob".into();
        profile.recipient.password = profile.password.clone();
        assert!(validate(&profile).is_err());
    }
}
