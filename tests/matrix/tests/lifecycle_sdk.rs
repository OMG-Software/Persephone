//! Real SDK calls at controlled barriers. All accounts/tokens/events are synthetic.
//! Tracked completion never certifies global SDK shutdown or key erasure.
#[path = "common/lifecycle_server.rs"]
mod lifecycle_server;

use matrix_sdk::{
    Client, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::EncryptionSettings,
    ruma::events::room::message::OriginalSyncRoomMessageEvent,
    sliding_sync::{SlidingSyncList, SlidingSyncMode, Version},
};
use persephone_test_matrix::{
    fixture_client_builder,
    lifecycle_fixture::{AdmissionError, LifecycleOwner, Phase, StopError},
};
use serde_json::{Value, json};
use std::{
    future::{Future, poll_fn},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::{sync::Notify, time};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const DEADLINE: Duration = Duration::from_secs(4);
const STOP_DEADLINE: Duration = Duration::from_millis(20);
const ROOM: &str = "!lifecycle:synthetic.test";

async fn restore(client: &Client) {
    client
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: "@lifecycle:synthetic.test".parse().unwrap(),
                device_id: "LIFECYCLE".into(),
            },
            tokens: SessionTokens {
                access_token: "synthetic-lifecycle-token".into(),
                refresh_token: None,
            },
        })
        .await
        .unwrap();
}

fn room_state() -> Value {
    json!([
        {"type":"m.room.create","state_key":"","sender":"@lifecycle:synthetic.test","event_id":"$create","origin_server_ts":1,"content":{"creator":"@lifecycle:synthetic.test","room_version":"11"}},
        {"type":"m.room.member","state_key":"@lifecycle:synthetic.test","sender":"@lifecycle:synthetic.test","event_id":"$join","origin_server_ts":2,"content":{"membership":"join"}}
    ])
}
fn event() -> Value {
    json!({"type":"m.room.message","sender":"@lifecycle:synthetic.test","event_id":"$message","origin_server_ts":3,"content":{"msgtype":"m.text","body":"synthetic lifecycle barrier"}})
}

async fn server(native: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/_matrix/client/versions")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"versions":["v1.15"],"unstable_features":{"org.matrix.simplified_msc3575":true}}))).mount(&server).await;
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
    let (verb, endpoint, response) = if native {
        (
            "POST",
            "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync",
            json!({
                "pos":"lifecycle-pos","lists":{"rooms":{"count":1,"ops":[{"op":"SYNC","range":[0,0],"room_ids":[ROOM]}]}},
                "rooms":{ROOM:{"initial":true,"joined_count":1,"invited_count":0,"required_state":room_state(),"timeline":[event()],"num_live":1,"prev_batch":"history","notification_count":0,"highlight_count":0}},
                "extensions":{"e2ee":{"device_lists":{"changed":[],"left":[]},"device_one_time_keys_count":{"signed_curve25519":50}},"to_device":{"events":[],"next_batch":"to-device-pos"}}
            }),
        )
    } else {
        (
            "GET",
            "/_matrix/client/v3/sync",
            json!({
                "next_batch":"classic-pos","rooms":{"join":{ROOM:{"state":{"events":room_state()},"timeline":{"events":[event()],"limited":false,"prev_batch":"history"},"ephemeral":{"events":[]},"account_data":{"events":[]}}}},
                "to_device":{"events":[]},"device_lists":{"changed":[],"left":[]},"device_one_time_keys_count":{"signed_curve25519":50}
            }),
        )
    };
    Mock::given(method(verb))
        .and(path(endpoint))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(1)
        .mount(&server)
        .await;
    server
}

async fn controlled_sync(native: bool) {
    let server = server(native).await;
    let client = fixture_client_builder(&server.uri())
        .unwrap()
        .build()
        .await
        .unwrap();
    restore(&client).await;
    let mut owner = LifecycleOwner::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let callback_finished = Arc::new(Notify::new());
    let published = Arc::new(AtomicBool::new(false));
    let sync_finished = Arc::new(AtomicBool::new(false));
    let callback = (
        owner.admission(),
        entered.clone(),
        release.clone(),
        callback_finished.clone(),
        published.clone(),
    );
    client.add_event_handler(move |_event: OriginalSyncRoomMessageEvent| {
        let (admission, entered, release, finished, published) = callback.clone();
        async move {
            let lease = admission.admit().unwrap();
            entered.notify_one();
            release.notified().await;
            assert_eq!(
                lease.commit(|| published.store(true, Ordering::SeqCst)),
                Err(AdmissionError::Closed)
            );
            finished.notify_one();
        }
    });
    let init = client.clone();
    owner
        .spawn(async move {
            init.encryption().wait_for_e2ee_initialization_tasks().await;
            Ok(())
        })
        .unwrap();
    let completed = sync_finished.clone();
    let stopping = owner.admission();
    if native {
        let sliding = client
            .sliding_sync("lifecycle")
            .unwrap()
            .version(Version::Native)
            .add_list(
                SlidingSyncList::builder("rooms")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=9)),
            )
            .with_all_extensions()
            .build()
            .await
            .unwrap();
        owner
            .spawn(async move {
                loop {
                    sliding.sync_once().await.map_err(|_| ())?;
                    completed.store(true, Ordering::SeqCst);
                    if stopping.stop_requested() {
                        break;
                    }
                }
                Ok(())
            })
            .unwrap();
    } else {
        let syncing = client.clone();
        owner
            .spawn(async move {
                loop {
                    syncing
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(10)))
                        .await
                        .map_err(|_| ())?;
                    completed.store(true, Ordering::SeqCst);
                    if stopping.stop_requested() {
                        break;
                    }
                }
                Ok(())
            })
            .unwrap();
    }
    time::timeout(DEADLINE, entered.notified())
        .await
        .expect("SDK callback entered");
    assert!(!sync_finished.load(Ordering::SeqCst));
    assert_eq!(
        owner.stop(STOP_DEADLINE).await.unwrap_err(),
        StopError::TimedOut
    );
    assert_eq!(owner.phase(), Phase::TimedOut);
    assert!(matches!(
        owner.admission().admit(),
        Err(AdmissionError::Closed)
    ));
    assert!(!sync_finished.load(Ordering::SeqCst));
    release.notify_one();
    time::timeout(DEADLINE, callback_finished.notified())
        .await
        .unwrap();
    let tracked = owner
        .stop(DEADLINE)
        .await
        .expect("retained SDK call must complete after callback release");
    assert!(sync_finished.load(Ordering::SeqCst));
    assert!(!published.load(Ordering::SeqCst));
    assert!(tracked.require_full_sdk_drain().is_err());
}

#[tokio::test]
async fn classic_sync_is_retained_until_handler_finishes_and_stale_update_is_fenced() {
    controlled_sync(false).await;
}

#[tokio::test]
async fn native_sync_and_e2ee_frame_are_retained_until_handler_finishes_and_stale_update_is_fenced()
{
    controlled_sync(true).await;
}

#[tokio::test]
async fn retained_init_wait_survives_stop_cancellation_and_pause_is_not_sdk_drain() {
    let server = lifecycle_server::LifecycleServer::start().await;
    let client = fixture_client_builder(&server.origin())
        .unwrap()
        .with_enable_share_history_on_invite(false)
        .with_encryption_settings(EncryptionSettings {
            auto_enable_backups: true,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    restore(&client).await;
    time::timeout(DEADLINE, server.entered.notified())
        .await
        .expect("real SDK backup-version request entered");
    let mut owner = LifecycleOwner::new();
    let pending = Arc::new(Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let (waiting, was_polled, done) = (client.clone(), pending.clone(), finished.clone());
    owner
        .spawn(async move {
            let encryption = waiting.encryption();
            let mut waiter = Box::pin(encryption.wait_for_e2ee_initialization_tasks());
            poll_fn(|cx| match waiter.as_mut().poll(cx) {
                Poll::Pending => {
                    was_polled.notify_one();
                    Poll::Pending
                }
                Poll::Ready(()) => Poll::Ready(()),
            })
            .await;
            done.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    time::timeout(DEADLINE, pending.notified()).await.unwrap();
    // Cancel only the caller's stop wait; the owner still holds the actual SDK waiter.
    assert!(
        time::timeout(STOP_DEADLINE, owner.stop(DEADLINE))
            .await
            .is_err()
    );
    assert_eq!(owner.phase(), Phase::Draining);
    assert!(!finished.load(Ordering::SeqCst));
    // The SDK took its handle into the retained waiter. A second call returns
    // immediately even though that initialization remains blocked on HTTP.
    let encryption = client.encryption();
    let mut second = Box::pin(encryption.wait_for_e2ee_initialization_tasks());
    assert!(poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_ready())).await);
    assert!(!finished.load(Ordering::SeqCst));
    // Negative control using MEMORY stores only: pause releases DB resources,
    // but does not join this in-flight encryption initialization request.
    time::timeout(DEADLINE, client.pause())
        .await
        .unwrap()
        .unwrap();
    assert!(!finished.load(Ordering::SeqCst));
    assert_eq!(
        owner.stop(STOP_DEADLINE).await.unwrap_err(),
        StopError::TimedOut
    );
    server.release.notify_one();
    let tracked = owner.stop(DEADLINE).await.unwrap();
    assert!(finished.load(Ordering::SeqCst));
    assert!(tracked.require_full_sdk_drain().is_err());
    time::timeout(DEADLINE, server.completed.notified())
        .await
        .unwrap();
    time::timeout(DEADLINE, server.close()).await.unwrap();
}
