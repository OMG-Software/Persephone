use std::time::Duration;

use anyhow::Result;
use matrix_sdk::{
    Client, SessionMeta, SessionTokens, authentication::matrix::MatrixSession,
    config::RequestConfig,
};
use persephone_test_matrix::{SyncMode, fixture_client_builder, sync_fixture::SyncPair};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const VERSIONS_PATH: &str = "/_matrix/client/versions";
const NATIVE_SYNC_PATH: &str = "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync";

fn capability(native: Option<bool>) -> Value {
    let mut response = json!({ "versions": ["v1.15"] });
    if let Some(enabled) = native {
        response["unstable_features"] = json!({
            "org.matrix.simplified_msc3575": enabled
        });
    }
    response
}

async fn mount_versions(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(VERSIONS_PATH))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn client(base_url: &str, label: &str, timeout: Option<Duration>) -> Result<Client> {
    let builder = fixture_client_builder(base_url)?;
    let builder = if let Some(timeout) = timeout {
        builder.request_config(RequestConfig::new().disable_retry().timeout(timeout))
    } else {
        builder
    };
    let client = builder.build().await?;
    client
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: format!("@fixture_{label}:synthetic.test").parse()?,
                device_id: format!("FIXTURE_{label}").into(),
            },
            tokens: SessionTokens {
                access_token: format!("synthetic-{label}-token"),
                refresh_token: None,
            },
        })
        .await?;
    Ok(client)
}

fn assert_discovered_without_sync(requests: &[wiremock::Request], context: &str) {
    assert!(
        requests
            .iter()
            .any(|request| request.method.as_str() == "GET" && request.url.path() == VERSIONS_PATH),
        "{context}: capability endpoint was not requested: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.url.path().ends_with("/sync")),
        "{context}: a sync request occurred before cycle: {requests:?}"
    );
}

fn sync_request_count(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|request| request.url.path().ends_with("/sync"))
        .count()
}

#[tokio::test]
async fn preferred_uses_advertised_mode_and_does_not_sync_before_cycle() -> Result<()> {
    for (label, native, expected_mode) in [
        ("enabled", Some(true), SyncMode::Sliding),
        ("absent", None, SyncMode::Classic),
        ("disabled", Some(false), SyncMode::Classic),
    ] {
        let server = MockServer::start().await;
        mount_versions(
            &server,
            ResponseTemplate::new(200).set_body_json(capability(native)),
        )
        .await;
        if native == Some(true) {
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/upload"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "one_time_key_counts": {"signed_curve25519": 50}
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/query"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "device_keys": {}, "failures": {}
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(NATIVE_SYNC_PATH))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;
        }

        let alice = client(&server.uri(), "alice", None).await?;
        let bob = client(&server.uri(), "bob", None).await?;
        let pair = SyncPair::preferred(&alice, &bob).await?;
        assert_eq!(pair.selected_mode(), expected_mode, "case {label}");

        let before_cycle = server.received_requests().await.unwrap();
        assert!(
            before_cycle.len() >= 2,
            "case {label}: both clients must discover"
        );
        assert_discovered_without_sync(&before_cycle, label);
        let syncs_before_cycle = sync_request_count(&before_cycle);

        if native == Some(true) {
            assert!(
                pair.cycle().await.is_err(),
                "503 should fail the native cycle"
            );
            let after_failure = server.received_requests().await.unwrap();
            assert!(
                sync_request_count(&after_failure) > syncs_before_cycle,
                "the failed cycle should issue a sync request"
            );
            assert!(
                after_failure
                    .iter()
                    .any(|request| request.url.path() == NATIVE_SYNC_PATH),
                "selected native mode must use the native endpoint"
            );
            assert!(
                after_failure
                    .iter()
                    .filter(|request| request.url.path().ends_with("/sync"))
                    .all(|request| request.url.path() == NATIVE_SYNC_PATH),
                "selected native mode must not fall back to classic sync"
            );

            assert!(pair.cycle().await.is_err(), "failed cycles must be refused");
            let after_retry = server.received_requests().await.unwrap();
            assert_eq!(
                sync_request_count(&after_retry),
                sync_request_count(&after_failure),
                "refused retry must not issue another sync request"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn discovery_errors_never_select_an_engine_or_fall_back() -> Result<()> {
    let cases = [
        (
            "http-401",
            ResponseTemplate::new(401).set_body_json(json!({
                "errcode": "M_UNKNOWN", "error": "synthetic failure"
            })),
            None,
        ),
        (
            "http-403",
            ResponseTemplate::new(403).set_body_json(json!({
                "errcode": "M_UNKNOWN", "error": "synthetic failure"
            })),
            None,
        ),
        (
            "http-404",
            ResponseTemplate::new(404).set_body_json(json!({
                "errcode": "M_UNKNOWN", "error": "synthetic failure"
            })),
            None,
        ),
        (
            "http-429",
            ResponseTemplate::new(429).set_body_json(json!({
                "errcode": "M_UNKNOWN", "error": "synthetic failure"
            })),
            None,
        ),
        (
            "http-503",
            ResponseTemplate::new(503).set_body_json(json!({
                "errcode": "M_UNKNOWN", "error": "synthetic failure"
            })),
            None,
        ),
        (
            "malformed-json",
            ResponseTemplate::new(200).set_body_raw("{", "application/json"),
            None,
        ),
        (
            "wrong-feature-type",
            ResponseTemplate::new(200).set_body_json(json!({
                "versions": ["v1.15"],
                "unstable_features": {"org.matrix.simplified_msc3575": "true"}
            })),
            None,
        ),
        (
            "timeout",
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(250))
                .set_body_json(capability(Some(true))),
            Some(Duration::from_millis(20)),
        ),
    ];

    for (label, response, timeout) in cases {
        let server = MockServer::start().await;
        mount_versions(&server, response).await;
        let alice = client(&server.uri(), "alice", timeout).await?;
        let bob = client(&server.uri(), "bob", timeout).await?;

        assert!(
            SyncPair::preferred(&alice, &bob).await.is_err(),
            "case {label} must fail capability discovery"
        );
        let requests = server.received_requests().await.unwrap();
        assert_discovered_without_sync(&requests, label);
    }
    Ok(())
}

#[tokio::test]
async fn preferred_rejects_clients_with_different_capabilities() -> Result<()> {
    let alice_server = MockServer::start().await;
    let bob_server = MockServer::start().await;
    mount_versions(
        &alice_server,
        ResponseTemplate::new(200).set_body_json(capability(Some(true))),
    )
    .await;
    mount_versions(
        &bob_server,
        ResponseTemplate::new(200).set_body_json(capability(None)),
    )
    .await;

    let alice = client(&alice_server.uri(), "alice", None).await?;
    let bob = client(&bob_server.uri(), "bob", None).await?;
    assert!(SyncPair::preferred(&alice, &bob).await.is_err());

    let alice_requests = alice_server.received_requests().await.unwrap();
    let bob_requests = bob_server.received_requests().await.unwrap();
    assert_discovered_without_sync(&alice_requests, "Alice capability");
    assert_discovered_without_sync(&bob_requests, "Bob capability");
    Ok(())
}
