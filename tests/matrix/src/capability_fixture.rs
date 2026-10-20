//! Capability-selected sync integration against disposable loopback servers.
//! No application coordinator, UI, or fallback after a running-engine failure.
use super::*;
use persephone_test_matrix::SyncMode;
use std::{collections::BTreeMap, ffi::OsString, io::Write};

#[derive(Serialize)]
struct Report {
    server: String,
    version: String,
    scenario: String,
    passed: bool,
    selected_mode: Option<SyncMode>,
    checks: BTreeMap<String, serde_json::Value>,
    both_logouts_confirmed: bool,
}

type Checks = BTreeMap<String, serde_json::Value>;

async fn messages(
    profile: &Profile,
    alice: &Client,
    bob: &Client,
    syncs: &SyncPair<'_>,
) -> Result<Checks> {
    let ar = SyncReceipts::new(alice);
    let br = SyncReceipts::new(bob);
    let (a_room, b_room) = Box::pin(create_verified_room(profile, alice, bob, syncs)).await?;
    let a_snapshot = security_snapshot(alice, bob).await?;
    let b_snapshot = security_snapshot(bob, alice).await?;
    encrypted_message(
        syncs,
        &br,
        alice,
        bob,
        &a_room,
        &b_room,
        "synthetic capability-selected Alice to Bob",
    )
    .await?;
    encrypted_message(
        syncs,
        &ar,
        bob,
        alice,
        &b_room,
        &a_room,
        "synthetic capability-selected Bob to Alice",
    )
    .await?;
    ensure!(
        a_snapshot == security_snapshot(alice, bob).await?
            && b_snapshot == security_snapshot(bob, alice).await?,
        "identity, keys or peer verification changed during selected sync"
    );
    let (native, classic) = syncs.completed_rounds();
    match syncs.selected_mode() {
        SyncMode::Sliding => ensure!(
            native > 0 && classic == 0,
            "native selection used classic sync"
        ),
        SyncMode::Classic => ensure!(
            classic > 0 && native == 0,
            "classic selection used native sync"
        ),
    }
    Ok(BTreeMap::from([
        (
            "mutual_sas_and_exact_bidirectional_decrypted_sync_receipts".into(),
            true.into(),
        ),
        (
            "identity_keys_and_peer_verification_preserved".into(),
            true.into(),
        ),
        ("native_rounds".into(), native.into()),
        ("classic_rounds".into(), classic.into()),
    ]))
}

async fn native_endpoint_is_unavailable(client: &Client, origin: &str) -> Result<()> {
    let session = client
        .matrix_auth()
        .session()
        .context("probe session missing")?;
    let http = persephone_test_matrix::policy_http_client_builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    let mut response = http
        .post(format!(
            "{origin}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"
        ))
        .bearer_auth(&session.tokens.access_token)
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await?;
    ensure!(
        response.status().as_u16() == 404,
        "disabled native endpoint did not return 404"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= 64 * 1024,
            "native control response too large"
        );
        body.extend_from_slice(&chunk);
    }
    let body: serde_json::Value = serde_json::from_slice(&body)?;
    ensure!(
        body["errcode"] == "M_UNRECOGNIZED",
        "disabled native endpoint did not return M_UNRECOGNIZED"
    );
    Ok(())
}

async fn exercise_preferred(profile: &Profile, scenario: &str) -> Result<Report> {
    let alice = make_client(profile).await?;
    let bob = make_client(profile).await?;
    let mut selected_mode = None;
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        if scenario == "expect-discovery-error" {
            // Fresh SDK clients: no login, cached successful versions or sync.
            ensure!(
                SyncPair::preferred(&alice, &bob).await.is_err(),
                "discovery error produced a sync engine"
            );
            return Ok(BTreeMap::from([(
                "discovery_failed_before_engine_creation".into(),
                true.into(),
            )]));
        }
        login_pair(profile, &alice, &bob).await?;
        let syncs = bounded(
            "capability-selected sync initialization",
            SyncPair::preferred(&alice, &bob),
        )
        .await?;
        selected_mode = Some(syncs.selected_mode());
        if scenario == "expect-native-error" {
            ensure!(
                syncs.selected_mode() == SyncMode::Sliding,
                "native error control did not select native"
            );
            ensure!(
                sync_pair(&syncs).await.is_err(),
                "native error control unexpectedly succeeded"
            );
            ensure!(
                syncs.selected_mode() == SyncMode::Sliding,
                "sync failure changed protocol"
            );
            ensure!(
                sync_pair(&syncs).await.is_err(),
                "failed engine admitted another cycle"
            );
            let (native, classic) = syncs.completed_rounds();
            ensure!(
                native == 0 && classic == 0,
                "failed sync was counted as successful"
            );
            return Ok(BTreeMap::from([
                ("sync_failed_without_changing_mode".into(), true.into()),
                ("failed_engine_refused_second_cycle".into(), true.into()),
                ("native_rounds".into(), native.into()),
                ("classic_rounds".into(), classic.into()),
            ]));
        }
        let mut checks = Box::pin(messages(profile, &alice, &bob, &syncs)).await?;
        if scenario == "discover-classic-only" {
            ensure!(
                syncs.selected_mode() == SyncMode::Classic,
                "classic-only instance selected native sync"
            );
            bounded(
                "confirm native endpoint unavailable",
                native_endpoint_is_unavailable(&alice, &profile.base_url),
            )
            .await?;
            checks.insert(
                "authenticated_native_endpoint_unrecognized".into(),
                true.into(),
            );
        }
        Ok(checks)
    })
    .await
    .unwrap_or_else(|_| Err(anyhow!("capability scenario timed out")));
    // Attempt both logouts independently, including failed authenticated cases.
    let al = logout(&alice).await;
    let bl = logout(&bob).await;
    let both_logouts_confirmed = al && bl;
    let no_login_expected = scenario == "expect-discovery-error";
    let passed = result.is_ok()
        && if no_login_expected {
            alice.user_id().is_none() && bob.user_id().is_none()
        } else {
            both_logouts_confirmed
        };
    Ok(Report {
        server: profile.name.clone(),
        version: profile.version.clone(),
        scenario: scenario.into(),
        passed,
        selected_mode,
        checks: result.unwrap_or_default(),
        both_logouts_confirmed,
    })
}

pub(super) async fn try_run(args: &[OsString]) -> Result<bool> {
    let Some(scenario) = args
        .get(2)
        .and_then(|a| a.to_str())
        .and_then(|a| a.strip_prefix("--"))
    else {
        return Ok(false);
    };
    if !matches!(
        scenario,
        "discover-sync"
            | "discover-classic-only"
            | "expect-discovery-error"
            | "expect-native-error"
    ) {
        return Ok(false);
    }
    ensure!(
        args.len() == 3,
        "capability worker requires a profile, new report and scenario"
    );
    let profiles: Vec<Profile> = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    ensure!(
        profiles.len() == 1,
        "capability worker requires exactly one disposable server"
    );
    let profile = &profiles[0];
    validate(profile)?;
    let output = Path::new(&args[1]);
    let local = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.local")
        .canonicalize()?;
    let parent = output
        .parent()
        .context("report needs parent")?
        .canonicalize()?;
    ensure!(parent.starts_with(local), "report must stay below .local");
    // Open atomically before contacting a server; never follow an existing link.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let result = Box::pin(exercise_preferred(profile, scenario)).await;
    let report = result.unwrap_or_else(|_| Report {
        server: profile.name.clone(),
        version: profile.version.clone(),
        scenario: scenario.into(),
        passed: false,
        selected_mode: None,
        checks: BTreeMap::new(),
        both_logouts_confirmed: false,
    });
    file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}: {} capability scenario {}",
        profile.name,
        if report.passed { "PASS" } else { "FAIL" },
        scenario
    );
    ensure!(
        report.passed,
        "capability scenario failed (payload-free report)"
    );
    Ok(true)
}
