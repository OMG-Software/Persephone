//! Disposable process-restart fixture, not an application session/unlock service.
use super::*;
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    ruma::{OwnedRoomId, events::room::encrypted::OriginalSyncRoomEncryptedEvent, serde::Raw},
};
use persephone_test_matrix::encrypted_fixture::{
    EncryptedClient, FixtureKey, open_encrypted_client, read_checkpoint, save_checkpoint,
};
use std::{collections::BTreeMap, ffi::OsString, io::Read, path::PathBuf};
use zeroize::Zeroizing;

const ALICE_OLD: &str = "synthetic restart old Alice to Bob 43A9";
const BOB_OLD: &str = "synthetic restart old Bob to Alice 79C2";
const ALICE_NEW: &str = "synthetic restart new Alice to Bob 51E8";
const BOB_NEW: &str = "synthetic restart new Bob to Alice 86D3";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeProfile {
    name: String,
    version: String,
    base_url: String,
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    session: MatrixSession,
    security: serde_json::Value,
    room_id: OwnedRoomId,
    incoming_id: OwnedEventId,
    incoming_body: String,
    incoming_wire: String,
    seed_pid: u32,
}

#[derive(Serialize)]
struct PhaseReport {
    server: String,
    version: String,
    phase: String,
    pid: u32,
    passed: bool,
    checks: BTreeMap<String, serde_json::Value>,
}

fn key_pair(missing: bool) -> Result<Option<(FixtureKey, FixtureKey)>> {
    // The test controller supplies raw keys over a pipe, never argv/env/files.
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::stdin().take(65).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() == if missing { 0 } else { 64 },
        "invalid key pipe length"
    );
    if missing {
        return Ok(None);
    }
    Ok(Some((
        FixtureKey::from_bytes(bytes[..32].try_into()?),
        FixtureKey::from_bytes(bytes[32..].try_into()?),
    )))
}

fn ignored_path(path: &Path, existing: bool) -> Result<PathBuf> {
    let local = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.local")
        .canonicalize()?;
    let resolved = if existing {
        path.canonicalize()?
    } else {
        path.parent()
            .context("path needs a parent")?
            .canonicalize()?
            .join(path.file_name().context("path needs a filename")?)
    };
    ensure!(
        resolved.starts_with(&local) && resolved != local,
        "fixture path must stay below .local"
    );
    Ok(resolved)
}

async fn clients(
    origin: &str,
    root: &Path,
    keys: &(FixtureKey, FixtureKey),
) -> Result<(EncryptedClient, EncryptedClient)> {
    let a = bounded(
        "open Alice encrypted stores",
        open_encrypted_client(origin, &root.join("alice"), Some(&keys.0)),
    )
    .await?;
    let b = bounded(
        "open Bob encrypted stores",
        open_encrypted_client(origin, &root.join("bob"), Some(&keys.1)),
    )
    .await?;
    Ok((a, b))
}

async fn wire(client: &Client, room: &Room, event_id: &OwnedEventId) -> Result<String> {
    let history = client
        .send(GetMessagesRequest::backward(room.room_id().to_owned()))
        .await?;
    for raw in history.chunk {
        let event: serde_json::Value = serde_json::from_str(raw.json().get())?;
        if event["event_id"] == event_id.as_str() {
            ensure!(
                event["type"] == "m.room.encrypted"
                    && event["content"]["algorithm"] == "m.megolm.v1.aes-sha2"
                    && event["content"].get("body").is_none(),
                "wire checkpoint is not encrypted"
            );
            return Ok(raw.json().get().to_owned());
        }
    }
    anyhow::bail!("encrypted checkpoint event missing")
}

async fn persist(
    own: &EncryptedClient,
    peer: &EncryptedClient,
    room: &Room,
    incoming: OwnedEventId,
    body: &str,
) -> Result<()> {
    let checkpoint = Checkpoint {
        session: own
            .client
            .matrix_auth()
            .session()
            .context("session missing")?,
        security: security_snapshot(&own.client, &peer.client).await?,
        room_id: room.room_id().to_owned(),
        incoming_wire: wire(&own.client, room, &incoming).await?,
        incoming_id: incoming,
        incoming_body: body.to_owned(),
        seed_pid: std::process::id(),
    };
    let bytes = Zeroizing::new(serde_json::to_vec(&checkpoint)?);
    save_checkpoint(own, &bytes).await?;
    ensure!(
        read_checkpoint(own).await? == *bytes,
        "checkpoint readback differs"
    );
    Ok(())
}

fn scan(root: &Path, needles: &[&[u8]]) -> Result<u64> {
    let mut files = 0;
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        ensure!(!path.is_symlink(), "unexpected store symlink");
        if path.is_dir() {
            files += scan(&path, needles)?;
            continue;
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for needle in needles {
            ensure!(!needle.is_empty(), "empty plaintext scan needle");
            ensure!(
                !bytes.windows(needle.len()).any(|w| w == *needle),
                "plaintext marker found in store artifacts"
            );
        }
        files += 1;
    }
    Ok(files)
}

async fn seed(
    profile: &Profile,
    root: &Path,
    keys: &(FixtureKey, FixtureKey),
) -> Result<BTreeMap<String, serde_json::Value>> {
    ensure!(!root.exists(), "seed stores already exist");
    std::fs::create_dir(root)?;
    let (alice, bob) = clients(&profile.base_url, root, keys).await?;
    let (a, b) = (&alice.client, &bob.client);
    login_pair(profile, a, b).await?;
    let syncs = SyncPair::new(a, b, ScenarioMode::SlidingThenClassic).await?;
    let ar = SyncReceipts::new(a);
    let br = SyncReceipts::new(b);
    let (a_room, b_room) = Box::pin(create_verified_room(profile, a, b, &syncs)).await?;
    let a_event = encrypted_message(&syncs, &br, a, b, &a_room, &b_room, ALICE_OLD).await?;
    let b_event = encrypted_message(&syncs, &ar, b, a, &b_room, &a_room, BOB_OLD).await?;
    persist(&alice, &bob, &a_room, b_event, BOB_OLD).await?;
    persist(&bob, &alice, &b_room, a_event, ALICE_OLD).await?;
    let (native, classic) = syncs.completed_rounds();
    ensure!(
        native > 0 && classic == 0,
        "seed did not exclusively use native sync"
    );
    let sa = a.matrix_auth().session().context("session missing")?;
    let sb = b.matrix_auth().session().context("session missing")?;
    let needles = [
        sa.tokens.access_token.as_bytes(),
        sb.tokens.access_token.as_bytes(),
        ALICE_OLD.as_bytes(),
        BOB_OLD.as_bytes(),
        keys.0.as_bytes(),
        keys.1.as_bytes(),
    ];
    let files = scan(root, &needles)?;
    ensure!(files >= 4, "store artifact scan empty");
    // No logout: the next process must restore these exact protected sessions.
    Ok(BTreeMap::from([
        (
            "mutual_sas_and_bidirectional_native_e2ee".into(),
            true.into(),
        ),
        (
            "encrypted_session_checkpoints_read_back".into(),
            true.into(),
        ),
        (
            "no_token_key_or_message_markers_in_store_artifacts".into(),
            true.into(),
        ),
        ("scanned_files".into(), files.into()),
        ("native_rounds".into(), native.into()),
        ("classic_rounds".into(), classic.into()),
    ]))
}

async fn restore(
    origin: &str,
    root: &Path,
    keys: &(FixtureKey, FixtureKey),
) -> Result<(EncryptedClient, EncryptedClient, Checkpoint, Checkpoint)> {
    eprintln!("restart-progress: opening-stores");
    let (a, b) = clients(origin, root, keys).await?;
    eprintln!("restart-progress: reading-checkpoints");
    let ac: Checkpoint = serde_json::from_slice(&Zeroizing::new(read_checkpoint(&a).await?))?;
    let bc: Checkpoint = serde_json::from_slice(&Zeroizing::new(read_checkpoint(&b).await?))?;
    ensure!(
        ac.seed_pid != std::process::id() && bc.seed_pid == ac.seed_pid,
        "not a new process"
    );
    // No password login and no new SAS flow in any restored worker.
    eprintln!("restart-progress: restoring-alice");
    a.client.restore_session(ac.session.clone()).await?;
    eprintln!("restart-progress: restoring-bob");
    b.client.restore_session(bc.session.clone()).await?;
    eprintln!("restart-progress: comparing-security");
    ensure!(
        ac.security == security_snapshot(&a.client, &b.client).await?
            && bc.security == security_snapshot(&b.client, &a.client).await?,
        "restored identity, device keys, or verification differ"
    );
    assert_trust(&a.client, &b.client, true).await?;
    Ok((a, b, ac, bc))
}

async fn decrypt_local(own: &EncryptedClient, checkpoint: &Checkpoint) -> Result<Room> {
    eprintln!("restart-progress: loading-room");
    let room = own
        .client
        .get_room(&checkpoint.room_id)
        .context("room not restored from state store")?;
    let raw: Raw<OriginalSyncRoomEncryptedEvent> = serde_json::from_str(&checkpoint.incoming_wire)?;
    eprintln!("restart-progress: decrypting-local-event");
    let event = room.decrypt_event(&raw, None).await?;
    eprintln!("restart-progress: checking-decrypted-event");
    ensure!(
        event.encryption_info().is_some(),
        "saved ciphertext was not decrypted"
    );
    let event: serde_json::Value = serde_json::from_str(event.raw().json().get())?;
    ensure!(
        event["event_id"] == checkpoint.incoming_id.as_str()
            && event["sender"] == checkpoint.security["peer_user"]
            && event["type"] == "m.room.message"
            && event["content"]["body"] == checkpoint.incoming_body,
        "local decryption differs from expected exact event"
    );
    Ok(room)
}

fn reserved_offline_origin() -> Result<(socket2::Socket, String)> {
    // Keep this bound socket alive without calling listen. Another process
    // cannot replace it with a server while we test local key recovery.
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())?;
    let address = socket
        .local_addr()?
        .as_socket()
        .context("offline socket address missing")?;
    ensure!(
        std::net::TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err(),
        "reserved offline port accepted a connection"
    );
    Ok((socket, format!("http://{address}")))
}

async fn recovered(
    profile: &ResumeProfile,
    root: &Path,
    keys: &(FixtureKey, FixtureKey),
    offline: bool,
) -> Result<BTreeMap<String, serde_json::Value>> {
    // A fresh client whose only origin is a closed loopback port. Redirects,
    // well-known discovery and proxies are disabled by the shared builder.
    // Offline proof never constructs a sync engine or fetches event history.
    let offline_socket = offline.then(reserved_offline_origin).transpose()?;
    let offline_url = offline_socket.as_ref().map(|(_, url)| url.as_str());
    let origin = offline_url.unwrap_or(&profile.base_url);
    let (alice, bob, ac, bc) = restore(origin, root, keys).await?;
    let a_room = decrypt_local(&alice, &ac).await?;
    let b_room = decrypt_local(&bob, &bc).await?;
    ensure!(ac.room_id == bc.room_id, "room checkpoints differ");
    let mut checks = BTreeMap::from([
        (
            "same_users_devices_keys_and_mutual_verification_before_sync".into(),
            true.into(),
        ),
        (
            "persisted_room_keys_decrypt_both_old_exact_events".into(),
            true.into(),
        ),
        ("password_login_and_sas_repeated".into(), false.into()),
    ]);
    if offline {
        checks.insert(
            "closed_origin_no_sync_or_history_requests".into(),
            true.into(),
        );
        return Ok(checks);
    }
    let (a, b) = (&alice.client, &bob.client);
    let syncs = SyncPair::new(a, b, ScenarioMode::SlidingThenClassic).await?;
    syncs.subscribe(&ac.room_id);
    let ar = SyncReceipts::new(a);
    let br = SyncReceipts::new(b);
    sync_pair(&syncs).await?;
    ensure!(
        ac.security == security_snapshot(a, b).await?
            && bc.security == security_snapshot(b, a).await?,
        "security state changed during resumed native sync"
    );
    encrypted_message(&syncs, &br, a, b, &a_room, &b_room, ALICE_NEW).await?;
    encrypted_message(&syncs, &ar, b, a, &b_room, &a_room, BOB_NEW).await?;
    ensure!(
        ac.security == security_snapshot(a, b).await?
            && bc.security == security_snapshot(b, a).await?,
        "security state changed after resumed messages"
    );
    let (native, classic) = syncs.completed_rounds();
    ensure!(
        native > 0 && classic == 0,
        "resume did not exclusively use native sync"
    );
    let needles = [
        ac.session.tokens.access_token.as_bytes(),
        bc.session.tokens.access_token.as_bytes(),
        ALICE_OLD.as_bytes(),
        BOB_OLD.as_bytes(),
        ALICE_NEW.as_bytes(),
        BOB_NEW.as_bytes(),
        keys.0.as_bytes(),
        keys.1.as_bytes(),
    ];
    let files = scan(root, &needles)?;
    let al = logout(a).await;
    let bl = logout(b).await;
    ensure!(al && bl, "both restored logouts must succeed");
    checks.extend([
        (
            "new_bidirectional_native_decrypted_sync_receipts".into(),
            true.into(),
        ),
        ("same_security_state_after_new_messages".into(), true.into()),
        (
            "no_token_key_or_message_markers_in_store_artifacts".into(),
            true.into(),
        ),
        ("both_logouts_confirmed".into(), true.into()),
        ("scanned_files".into(), files.into()),
        ("native_rounds".into(), native.into()),
        ("classic_rounds".into(), classic.into()),
    ]);
    Ok(checks)
}

pub(super) async fn try_run(args: &[OsString]) -> Result<bool> {
    if !args
        .get(2)
        .is_some_and(|arg| arg.to_string_lossy().starts_with("--restart-"))
    {
        return Ok(false);
    }
    ensure!(
        args.len() == 4,
        "restart worker requires profile, report, phase, and store root"
    );
    let phase = args[2]
        .to_str()
        .context("invalid phase")?
        .strip_prefix("--restart-")
        .context("invalid phase")?;
    ensure!(
        matches!(
            phase,
            "seed" | "wrong-key" | "missing-key" | "offline" | "resume"
        ),
        "unknown restart phase"
    );
    let output = ignored_path(Path::new(&args[1]), false)?;
    ensure!(
        std::fs::symlink_metadata(&output).is_err(),
        "refusing to overwrite a result"
    );
    let root = ignored_path(Path::new(&args[3]), phase != "seed")?;
    let bytes = std::fs::read(&args[0])?;
    let (profile, seed_profile) = if phase == "seed" {
        let mut profiles: Vec<Profile> = serde_json::from_slice(&bytes)?;
        ensure!(profiles.len() == 1, "restart worker requires one server");
        let p = profiles.remove(0);
        validate(&p)?;
        (
            ResumeProfile {
                name: p.name.clone(),
                version: p.version.clone(),
                base_url: p.base_url.clone(),
            },
            Some(p),
        )
    } else {
        let mut profiles: Vec<ResumeProfile> = serde_json::from_slice(&bytes)?;
        ensure!(profiles.len() == 1, "restart worker requires one server");
        let p = profiles.remove(0);
        validate(&Profile {
            name: p.name.clone(),
            version: p.version.clone(),
            base_url: p.base_url.clone(),
            username: "p0_alice".into(),
            password: "guard-a".into(),
            recipient: Account {
                username: "p0_bob".into(),
                password: "guard-b".into(),
            },
        })?;
        (p, None)
    };
    let keys = key_pair(phase == "missing-key")?;
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        match phase {
            "seed" => {
                Box::pin(seed(
                    seed_profile.as_ref().context("seed profile missing")?,
                    &root,
                    keys.as_ref().context("keys missing")?,
                ))
                .await
            }
            "wrong-key" => {
                let keys = keys.as_ref().context("keys missing")?;
                for (name, key) in [("alice", &keys.0), ("bob", &keys.1)] {
                    ensure!(
                        open_encrypted_client(&profile.base_url, &root.join(name), Some(key))
                            .await
                            .is_err(),
                        "wrong key accepted"
                    );
                }
                Ok(BTreeMap::from([(
                    "both_wrong_keys_rejected".into(),
                    true.into(),
                )]))
            }
            "missing-key" => {
                for name in ["alice", "bob", "missing-key-must-not-create"] {
                    ensure!(
                        open_encrypted_client(&profile.base_url, &root.join(name), None)
                            .await
                            .is_err(),
                        "missing key accepted"
                    );
                }
                ensure!(
                    !root.join("missing-key-must-not-create").exists(),
                    "missing key created a store"
                );
                Ok(BTreeMap::from([(
                    "missing_keys_rejected_without_plaintext_fallback".into(),
                    true.into(),
                )]))
            }
            "offline" | "resume" => {
                Box::pin(recovered(
                    &profile,
                    &root,
                    keys.as_ref().context("keys missing")?,
                    phase == "offline",
                ))
                .await
            }
            _ => unreachable!(),
        }
    })
    .await
    .unwrap_or_else(|_| Err(anyhow!("restart phase timed out")));
    let passed = result.is_ok();
    let report = PhaseReport {
        server: profile.name,
        version: profile.version,
        phase: phase.to_owned(),
        pid: std::process::id(),
        passed,
        checks: result.unwrap_or_default(),
    };
    use std::io::Write;
    let mut report_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    report_file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    ensure!(
        passed,
        "encrypted restart phase failed (sanitized; no SDK payload logged)"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_origin_reserves_a_port_without_accepting_connections() {
        let (_socket, url) = reserved_offline_origin().unwrap();
        let address = url.strip_prefix("http://").unwrap();
        assert!(std::net::TcpListener::bind(address).is_err());
        assert!(
            std::net::TcpStream::connect_timeout(
                &address.parse().unwrap(),
                Duration::from_millis(100)
            )
            .is_err()
        );
    }
}
