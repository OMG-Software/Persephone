//! P0 investigation only. Not an application sync coordinator.

pub mod encrypted_fixture;
pub mod lifecycle_fixture;
pub mod sync_fixture;
pub mod sync_receipts;

use matrix_sdk::{Client, ClientBuilder, config::RequestConfig, sliding_sync::VersionBuilder};
use std::time::Duration;

/// The single HTTP client policy for every Matrix connection: TLS 1.3 only, no
/// ambient proxy and no redirects. reqwest 0.13 compiles rustls with TLS 1.2
/// support unconditionally, so TLS 1.2 is refused here at configuration time;
/// it is then absent from the ClientHello. Build every HTTP client from this.
pub fn policy_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Sliding,
    Classic,
}

/// Require successful capability discovery; never turn transport/parser failures
/// into an unsupported-feature result. A fresh client avoids a previous cache
/// masking a fixture failure; production cache provenance needs its own policy.
pub async fn discover(client: &Client) -> matrix_sdk::HttpResult<SyncMode> {
    let supported = client.supported_versions().await?;
    Ok(
        if VersionBuilder::DiscoverNative
            .build(Some(&supported))
            .is_ok()
        {
            SyncMode::Sliding
        } else {
            SyncMode::Classic
        },
    )
}

pub fn fixture_client_builder(base_url: &str) -> anyhow::Result<ClientBuilder> {
    Ok(Client::builder()
        .homeserver_url(base_url)
        .respect_login_well_known(false)
        .disable_well_known_lookup(true)
        .http_client(policy_http_client_builder().build()?)
        .request_config(
            RequestConfig::new()
                .disable_retry()
                .timeout(Duration::from_secs(3)),
        ))
}

pub async fn client(base_url: &str) -> anyhow::Result<Client> {
    Ok(fixture_client_builder(base_url)?.build().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn server(response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn given_native_capability_when_discovered_then_sliding_is_preferred() {
        let server = server(ResponseTemplate::new(200).set_body_json(json!({
            "versions": ["v1.15"],
            "unstable_features": {"org.matrix.simplified_msc3575": true}
        })))
        .await;
        assert_eq!(
            discover(&client(&server.uri()).await.unwrap())
                .await
                .unwrap(),
            SyncMode::Sliding
        );
    }

    #[tokio::test]
    async fn given_valid_absent_or_disabled_capability_then_classic_is_selected() {
        for features in [
            json!({}),
            json!({"org.matrix.simplified_msc3575": false}),
            json!({"org.matrix.msc4186": true}),
        ] {
            let server = server(ResponseTemplate::new(200).set_body_json(json!({
                "versions": ["v1.15"], "unstable_features": features
            })))
            .await;
            assert_eq!(
                discover(&client(&server.uri()).await.unwrap())
                    .await
                    .unwrap(),
                SyncMode::Classic
            );
        }
    }

    #[tokio::test]
    async fn given_http_failures_when_discovered_then_no_fallback_is_returned() {
        for code in [401, 403, 404, 429, 500, 503] {
            let server = server(ResponseTemplate::new(code).set_body_json(json!({
                "errcode": "M_UNKNOWN", "error": "synthetic failure"
            })))
            .await;
            assert!(
                discover(&client(&server.uri()).await.unwrap())
                    .await
                    .is_err(),
                "status {code}"
            );
        }
    }

    #[tokio::test]
    async fn given_malformed_or_wrong_feature_type_then_discovery_fails_closed() {
        for body in [
            "{",
            r#"{"versions":"invalid"}"#,
            r#"{"versions":["v1.15"],"unstable_features":{"org.matrix.simplified_msc3575":"true"}}"#,
        ] {
            let server =
                server(ResponseTemplate::new(200).set_body_raw(body, "application/json")).await;
            assert!(
                discover(&client(&server.uri()).await.unwrap())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn given_discovery_timeout_then_no_classic_fallback() {
        let server = server(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(json!({"versions":["v1.15"]})),
        )
        .await;
        let client = Client::builder()
            .homeserver_url(server.uri())
            .http_client(policy_http_client_builder().build().unwrap())
            .request_config(
                RequestConfig::new()
                    .disable_retry()
                    .timeout(Duration::from_millis(30)),
            )
            .build()
            .await
            .unwrap();
        assert!(discover(&client).await.is_err());
    }

    #[tokio::test]
    async fn given_failed_versions_when_sdk_convenience_helper_runs_then_it_suppresses_error() {
        let server = server(ResponseTemplate::new(503)).await;
        let client = client(&server.uri()).await.unwrap();
        assert!(client.available_sliding_sync_versions().await.is_empty());
        assert!(discover(&client).await.is_err());
    }

    #[tokio::test]
    async fn given_invalid_tls_then_discovery_does_not_downgrade() {
        let server = server(ResponseTemplate::new(200)).await;
        // TLS client pointed at a plain-HTTP fixture: deterministic handshake
        // failure, not a full certificate/hostname validation suite.
        let url = server.uri().replacen("http://", "https://", 1);
        assert!(discover(&client(&url).await.unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn given_classic_only_server_then_sdk_uses_classic_and_never_posts_sliding() {
        use matrix_sdk::{
            SessionMeta, SessionTokens, authentication::matrix::MatrixSession, config::SyncSettings,
        };
        let server =
            server(ResponseTemplate::new(200).set_body_json(json!({"versions":["v1.15"]}))).await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/keys/upload"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"one_time_key_counts":{"signed_curve25519":50}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/keys/query"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"device_keys":{},"failures":{}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch":"classic-checkpoint", "rooms":{}, "to_device":{"events":[]},
                "device_lists":{"changed":[],"left":[]},
                "device_one_time_keys_count":{"signed_curve25519":50}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri()).await.unwrap();
        c.restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: "@p0_alice:synthetic.test".parse().unwrap(),
                device_id: "P0DEVICE".into(),
            },
            tokens: SessionTokens {
                access_token: "synthetic-fixture-token".into(),
                refresh_token: None,
            },
        })
        .await
        .unwrap();
        let response = match discover(&c).await.unwrap() {
            SyncMode::Classic => c
                .sync_once(
                    SyncSettings::default()
                        .set_presence(matrix_sdk::ruma::presence::PresenceState::Offline),
                )
                .await
                .unwrap(),
            SyncMode::Sliding => panic!("unexpected protocol downgrade decision"),
        };
        assert_eq!(response.next_batch, "classic-checkpoint");
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path().contains("msc3575"))
        );
    }

    #[tokio::test]
    async fn given_response_processing_when_outer_sync_is_aborted_then_processing_can_continue() {
        use matrix_sdk::{
            SessionMeta, SessionTokens,
            authentication::matrix::MatrixSession,
            ruma::events::room::message::OriginalSyncRoomMessageEvent,
            sliding_sync::{SlidingSyncList, SlidingSyncMode, Version},
        };
        use std::sync::Arc;
        use tokio::sync::Notify;
        let server = server(ResponseTemplate::new(200).set_body_json(json!({
            "versions":["v1.15"], "unstable_features":{"org.matrix.simplified_msc3575":true}
        })))
        .await;
        let room_id = "!p0:synthetic.test";
        Mock::given(method("POST")).and(path("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pos":"sliding-position", "lists":{"rooms":{"count":1,"ops":[{"op":"SYNC","range":[0,0],"room_ids":[room_id]}]}},
                "rooms":{room_id:{"initial":true,"name":"synthetic","joined_count":1,"invited_count":0,
                    "required_state":[
                        {"type":"m.room.create","state_key":"","sender":"@p0_alice:synthetic.test","event_id":"$create:synthetic.test","origin_server_ts":1,"content":{"creator":"@p0_alice:synthetic.test","room_version":"11"}},
                        {"type":"m.room.member","state_key":"@p0_alice:synthetic.test","sender":"@p0_alice:synthetic.test","event_id":"$join:synthetic.test","origin_server_ts":2,"content":{"membership":"join"}}
                    ],
                    "timeline":[{"type":"m.room.message","sender":"@p0_alice:synthetic.test","event_id":"$message:synthetic.test","origin_server_ts":3,"content":{"msgtype":"m.text","body":"synthetic"}}],
                    "num_live":1,"prev_batch":"history-position","notification_count":0,"highlight_count":0
                }}, "extensions":{}
            }))).mount(&server).await;
        let c = client(&server.uri()).await.unwrap();
        c.restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: "@p0_alice:synthetic.test".parse().unwrap(),
                device_id: "P0DEVICE".into(),
            },
            tokens: SessionTokens {
                access_token: "synthetic-fixture-token".into(),
                refresh_token: None,
            },
        })
        .await
        .unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(Notify::new());
        let signals = (entered.clone(), release.clone(), completed.clone());
        c.add_event_handler(move |_event: OriginalSyncRoomMessageEvent| {
            let (entered, release, completed) = signals.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                completed.notify_one();
            }
        });
        // No E2EE extensions here: this narrowly observes response-handler
        // ownership, not encryption processing or whole-client shutdown.
        let sliding = c
            .sliding_sync("p0-drain")
            .unwrap()
            .version(Version::Native)
            .add_list(
                SlidingSyncList::builder("rooms")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=9)),
            )
            .build()
            .await
            .unwrap();
        let task = tokio::spawn(async move { sliding.sync_once().await });
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.notify_one();
        // Completed after the outer task is confirmed cancelled: a behavioural
        // counterexample to treating abort+await as a complete SDK drain.
        tokio::time::timeout(Duration::from_secs(3), completed.notified())
            .await
            .unwrap();
    }
}
