use std::{fs, path::Path};

use matrix_sdk_base::{
    event_cache::{Event, store::EventCacheStore},
    media::{
        MediaFormat, MediaRequestParameters,
        store::{IgnoreMediaRetentionPolicy, MediaStore},
    },
    store::{
        QueuedRequestKind, SerializableEventContent, StateStore, StateStoreDataKey,
        StateStoreDataValue,
    },
};
use matrix_sdk_crypto::{OlmMachineBuilder, store::CryptoStore};
use matrix_sdk_sqlite::{
    SqliteCryptoStore, SqliteEventCacheStore, SqliteMediaStore, SqliteStateStore,
};
use ruma::{
    MilliSecondsSinceUnixEpoch, TransactionId, device_id,
    events::{AnyMessageLikeEventContent, AnySyncTimelineEvent, room::MediaSource},
    mxc_uri, room_id,
    serde::Raw,
    user_id,
};
use tempfile::TempDir;

const STORE_KEY: &[u8; 32] = b"synthetic-storage-probe-key-0001";
const WRONG_KEY: &[u8; 32] = b"synthetic-storage-probe-key-0002";
const MARKER: &[u8] = b"PERSEPHONE_SYNTHETIC_PRIVATE_MARKER_4F28B1";

#[derive(Debug, Default)]
struct ArtifactScan {
    scanned_files: Vec<String>,
    marker_files: Vec<String>,
    private_pattern_files: Vec<String>,
    total_bytes: usize,
}

fn scan_artifacts(root: &Path) -> ArtifactScan {
    scan_artifacts_with_patterns(root, &[])
}

fn scan_artifacts_with_patterns(root: &Path, patterns: &[Vec<u8>]) -> ArtifactScan {
    scan_artifacts_under(root, root, patterns)
}

fn scan_artifacts_under(root: &Path, dir: &Path, patterns: &[Vec<u8>]) -> ArtifactScan {
    let mut scan = ArtifactScan::default();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            let nested = scan_artifacts_under(root, &path, patterns);
            scan.scanned_files.extend(nested.scanned_files);
            scan.marker_files.extend(nested.marker_files);
            scan.private_pattern_files
                .extend(nested.private_pattern_files);
            scan.total_bytes += nested.total_bytes;
        } else {
            // SQLite WAL/SHM files may disappear during close/checkpoint. Do
            // not suppress permission, sharing, or other unexpected failures.
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("failed to scan {}: {error}", path.display()),
            };
            scan.total_bytes += bytes.len();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            scan.scanned_files.push(name.clone());
            if bytes.windows(MARKER.len()).any(|window| window == MARKER) {
                scan.marker_files.push(name);
            }
            if patterns
                .iter()
                .filter(|pattern| !pattern.is_empty())
                .any(|pattern| {
                    bytes
                        .windows(pattern.len())
                        .any(|window| window == pattern.as_slice())
                })
            {
                scan.private_pattern_files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    scan.scanned_files.sort();
    scan.marker_files.sort();
    scan
}

fn collect_pickle_patterns(value: &serde_json::Value, patterns: &mut Vec<Vec<u8>>) {
    match value {
        serde_json::Value::String(value) if value.len() >= 16 => {
            patterns.push(value.as_bytes().to_vec());
        }
        serde_json::Value::Array(values)
            if values.len() >= 16
                && values.iter().all(|value| {
                    value
                        .as_u64()
                        .is_some_and(|number| number <= u8::MAX as u64)
                }) =>
        {
            let bytes: Vec<u8> = values
                .iter()
                .map(|value| value.as_u64().unwrap() as u8)
                .collect();
            patterns.push(bytes);
            patterns.push(serde_json::to_vec(value).unwrap());
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_pickle_patterns(value, patterns);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_pickle_patterns(value, patterns);
            }
        }
        _ => {}
    }
}

fn private_account_pickle_patterns(account_pickle: &serde_json::Value) -> Vec<Vec<u8>> {
    let pickle = &account_pickle["pickle"];
    let mut patterns = Vec::new();
    for field in ["signing_key", "diffie_hellman_key"] {
        let private_key = pickle
            .get(field)
            .unwrap_or_else(|| panic!("account pickle omitted private field {field}"));
        collect_pickle_patterns(private_key, &mut patterns);
    }
    patterns.retain(|pattern| pattern.len() >= 16);
    patterns.sort();
    patterns.dedup();
    assert!(
        !patterns.is_empty(),
        "no recognizable private account pickle patterns were produced"
    );
    patterns
}

fn assert_scan_is_clean(scan: &ArtifactScan, database: &str) {
    assert!(scan.total_bytes > 0, "no artifact bytes scanned: {scan:?}");
    assert!(
        scan.scanned_files
            .iter()
            .any(|name| name.ends_with(database)),
        "database was not scanned: {scan:?}"
    );
    assert!(
        scan.marker_files.is_empty(),
        "marker found in artifacts: {scan:?}"
    );
    println!(
        "artifact scan: files={:?}, bytes={}, marker_files={:?}",
        scan.scanned_files, scan.total_bytes, scan.marker_files
    );
}

fn assert_live_sqlite_sidecars(scan: &ArtifactScan, database: &str) {
    assert_scan_is_clean(scan, database);
    assert!(scan.scanned_files.iter().any(|name| name.ends_with("-wal")));
    assert!(scan.scanned_files.iter().any(|name| name.ends_with("-shm")));
}

#[tokio::test]
async fn encrypted_olm_account_pickle_is_absent_from_database_and_sidecars() {
    let dir = TempDir::new().unwrap();
    let store = SqliteCryptoStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    let machine = OlmMachineBuilder::new(
        user_id!("@storage-probe:example.org"),
        device_id!("P0STORAGE"),
    )
    .with_crypto_store(store.clone())
    .build()
    .await
    .unwrap();
    let account = store.load_account().await.unwrap().unwrap();
    let account_pickle = serde_json::to_value(account.pickle()).unwrap();
    let private_patterns = private_account_pickle_patterns(&account_pickle);
    let private_pickle_bytes = serde_json::to_vec(&account_pickle).unwrap();

    store
        .set_custom_value("synthetic-private-value", MARKER.to_vec())
        .await
        .unwrap();
    assert_eq!(
        store
            .get_custom_value("synthetic-private-value")
            .await
            .unwrap()
            .unwrap(),
        MARKER
    );
    let live_scan = scan_artifacts_with_patterns(dir.path(), &private_patterns);
    assert_live_sqlite_sidecars(&live_scan, "matrix-sdk-crypto.sqlite3");
    assert!(
        live_scan.private_pattern_files.is_empty(),
        "real SDK private account pickle patterns appeared in encrypted artifacts: {live_scan:?}"
    );

    // Positive control: store the same real SDK-generated private-account pickle
    // as a custom value in an intentionally unkeyed SDK store. The same artifact
    // scanner must find its private serialization patterns there.
    let plaintext_dir = TempDir::new().unwrap();
    let plaintext_store = SqliteCryptoStore::open_with_key(plaintext_dir.path(), None)
        .await
        .unwrap();
    plaintext_store
        .set_custom_value("private-account-pickle-control", private_pickle_bytes)
        .await
        .unwrap();
    let plaintext_scan = scan_artifacts_with_patterns(plaintext_dir.path(), &private_patterns);
    assert!(
        !plaintext_scan.private_pattern_files.is_empty(),
        "scanner failed to detect the real private account pickle in the plaintext control: {plaintext_scan:?}"
    );
    plaintext_store.close().await.unwrap();

    drop(account);
    drop(machine);
    store.close().await.unwrap();
    let closed_scan = scan_artifacts_with_patterns(dir.path(), &private_patterns);
    assert_scan_is_clean(&closed_scan, "matrix-sdk-crypto.sqlite3");
    assert!(
        closed_scan.private_pattern_files.is_empty(),
        "real SDK private account pickle patterns appeared after close: {closed_scan:?}"
    );
    println!(
        "real crypto-account scan: private_patterns={}, plaintext_control_hits={:?}, encrypted_live_hits={:?}, encrypted_closed_hits={:?}",
        private_patterns.len(),
        plaintext_scan.private_pattern_files,
        live_scan.private_pattern_files,
        closed_scan.private_pattern_files,
    );

    let db = dir.path().join("matrix-sdk-crypto.sqlite3");
    let bytes = fs::read(&db).unwrap();
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let schema: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'kv'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let custom_key: String = conn
        .query_row(
            "SELECT key FROM kv WHERE key = 'synthetic-private-value'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(schema.contains("CREATE TABLE"));
    assert_eq!(custom_key, "synthetic-private-value");
    println!("plaintext sqlite metadata: schema={schema}; custom key={custom_key}");
}

#[tokio::test]
async fn encrypted_state_sync_token_is_absent_from_sqlite_artifacts() {
    let dir = TempDir::new().unwrap();
    let store = SqliteStateStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    store
        .set_kv_data(
            StateStoreDataKey::SyncToken,
            StateStoreDataValue::SyncToken(String::from_utf8(MARKER.to_vec()).unwrap()),
        )
        .await
        .unwrap();
    assert_live_sqlite_sidecars(&scan_artifacts(dir.path()), "matrix-sdk-state.sqlite3");
    store.close().await.unwrap();
    assert_scan_is_clean(&scan_artifacts(dir.path()), "matrix-sdk-state.sqlite3");

    let db = dir.path().join("matrix-sdk-state.sqlite3");
    let bytes = fs::read(&db).unwrap();
    assert!(bytes.starts_with(b"SQLite format 3\0"));
}

#[tokio::test]
async fn encrypted_send_queue_message_content_is_absent_from_database_and_sidecars() {
    let dir = TempDir::new().unwrap();
    let store = SqliteStateStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    let room_id = room_id!("!probe-send-queue:example.org");
    let raw = Raw::<AnyMessageLikeEventContent>::from_json_string(format!(
        r#"{{"msgtype":"m.text","body":"{}"}}"#,
        String::from_utf8(MARKER.to_vec()).unwrap()
    ))
    .unwrap();
    let content = SerializableEventContent::from_raw(raw, "m.room.message".to_owned());
    store
        .save_send_queue_request(
            room_id,
            TransactionId::new(),
            MilliSecondsSinceUnixEpoch::now(),
            QueuedRequestKind::Event { content },
            0,
        )
        .await
        .unwrap();
    let requests = store.load_send_queue_requests(room_id).await.unwrap();
    assert_eq!(requests.len(), 1);
    let serialized = serde_json::to_vec(&requests[0].kind).unwrap();
    assert!(
        serialized
            .windows(MARKER.len())
            .any(|window| window == MARKER)
    );
    assert_live_sqlite_sidecars(&scan_artifacts(dir.path()), "matrix-sdk-state.sqlite3");

    store.close().await.unwrap();
    assert_scan_is_clean(&scan_artifacts(dir.path()), "matrix-sdk-state.sqlite3");
}

#[tokio::test]
async fn encrypted_event_cache_content_is_absent_from_database_and_sidecars() {
    let dir = TempDir::new().unwrap();
    let store = SqliteEventCacheStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    let room_id = room_id!("!probe:example.org");
    let raw: Raw<AnySyncTimelineEvent> = serde_json::from_str(&format!(
        r#"{{"event_id":"$probe:example.org","sender":"@probe:example.org","origin_server_ts":1,"type":"m.room.message","content":{{"msgtype":"m.text","body":"{}"}}}}"#,
        String::from_utf8(MARKER.to_vec()).unwrap()
    ))
    .unwrap();
    store
        .save_event(room_id, Event::from_plaintext(raw))
        .await
        .unwrap();
    let events = store.get_room_events(room_id, None, None).await.unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        serde_json::to_vec(&events[0])
            .unwrap()
            .windows(MARKER.len())
            .any(|window| window == MARKER)
    );
    assert_live_sqlite_sidecars(
        &scan_artifacts(dir.path()),
        "matrix-sdk-event-cache.sqlite3",
    );

    store.close().await.unwrap();
    assert_scan_is_clean(
        &scan_artifacts(dir.path()),
        "matrix-sdk-event-cache.sqlite3",
    );
}

#[tokio::test]
async fn encrypted_media_content_is_absent_from_database_and_sidecars() {
    let dir = TempDir::new().unwrap();
    let store = SqliteMediaStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    let request = MediaRequestParameters {
        source: MediaSource::Plain(mxc_uri!("mxc://example.org/probe-media").to_owned()),
        format: MediaFormat::File,
    };
    store
        .add_media_content(&request, MARKER.to_vec(), IgnoreMediaRetentionPolicy::No)
        .await
        .unwrap();
    assert_eq!(
        store.get_media_content(&request).await.unwrap().unwrap(),
        MARKER
    );
    assert_live_sqlite_sidecars(&scan_artifacts(dir.path()), "matrix-sdk-media.sqlite3");

    store.close().await.unwrap();
    assert_scan_is_clean(&scan_artifacts(dir.path()), "matrix-sdk-media.sqlite3");
}

#[tokio::test]
async fn wrong_key_fails_and_correct_key_recovers_real_olm_account() {
    let dir = TempDir::new().unwrap();
    let store = SqliteCryptoStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    let machine = OlmMachineBuilder::new(
        user_id!("@storage-probe:example.org"),
        device_id!("P0STORAGE"),
    )
    .with_crypto_store(store.clone())
    .build()
    .await
    .unwrap();
    let account = store.load_account().await.unwrap().unwrap();
    let expected_pickle = serde_json::to_value(account.pickle()).unwrap();
    drop(account);
    drop(machine);
    store.close().await.unwrap();

    assert!(
        SqliteCryptoStore::open_with_key(dir.path(), Some(WRONG_KEY))
            .await
            .is_err()
    );

    let store = SqliteCryptoStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    let restored_account = store.load_account().await.unwrap().unwrap();
    // Never include private pickle contents in an assertion's failure output.
    assert!(
        serde_json::to_value(restored_account.pickle()).unwrap() == expected_pickle,
        "correct-key reload did not preserve the generated private account"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn missing_key_does_not_enforce_locked_state_and_can_write_plaintext() {
    let dir = TempDir::new().unwrap();
    let store = SqliteCryptoStore::open_with_key(dir.path(), Some(STORE_KEY))
        .await
        .unwrap();
    store
        .set_custom_value("synthetic-private-value", MARKER.to_vec())
        .await
        .unwrap();
    drop(store);

    let store = SqliteCryptoStore::open_with_key(dir.path(), None)
        .await
        .unwrap();
    store
        .set_custom_value("missing-key-write", MARKER.to_vec())
        .await
        .unwrap();
    store.close().await.unwrap();
    drop(store);

    let scan = scan_artifacts(dir.path());
    assert!(scan.total_bytes > 0);
    assert!(
        !scan.marker_files.is_empty(),
        "expected the deliberate plaintext write: {scan:?}"
    );
    println!("missing-key plaintext write artifacts: {scan:?}");
}
