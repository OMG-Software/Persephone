//! Regression checks for behavior that retained, unmaintained dependencies
//! rely on. See docs/evidence/dependencies.md for each disposition.

use matrix_sdk::Client;
use persephone_test_matrix::client;
use std::{
    any::TypeId,
    hash::{Hash, Hasher},
};
use wiremock::MockServer;

#[derive(Default)]
struct WriteRecorder {
    lengths: Vec<usize>,
}

impl Hasher for WriteRecorder {
    fn write(&mut self, bytes: &[u8]) {
        self.lengths.push(bytes.len());
    }

    fn finish(&self) -> u64 {
        0
    }
}

fn type_id_write_lengths(type_id: TypeId) -> Vec<usize> {
    let mut recorder = WriteRecorder::default();
    type_id.hash(&mut recorder);
    recorder.lengths
}

/// anymap2 0.13.0 keys matrix-sdk's event-handler context map with a hasher
/// that asserts it receives exactly one 8-byte write. `TypeId` is wider than
/// that, so the assertion holds only while std hashes it as a single u64.
/// A toolchain change here would turn context registration into a panic.
#[test]
fn given_current_toolchain_when_type_id_is_hashed_then_anymap2_receives_one_u64_write() {
    for type_id in [
        TypeId::of::<usize>(),
        TypeId::of::<()>(),
        TypeId::of::<str>(),
        TypeId::of::<Vec<u8>>(),
        TypeId::of::<String>(),
        TypeId::of::<Client>(),
    ] {
        assert_eq!(type_id_write_lengths(type_id), [8]);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct SyntheticContext(u32);

/// Exercises anymap2 insertion and same-type replacement through the public
/// matrix-sdk API the application would use, without any homeserver request.
#[tokio::test]
async fn given_offline_client_when_event_handler_contexts_are_added_then_sdk_map_accepts_them() {
    let server = MockServer::start().await;
    let client = client(&server.uri()).await.unwrap();

    client.add_event_handler_context(SyntheticContext(1));
    client.add_event_handler_context(SyntheticContext(2));
    client.add_event_handler_context(String::from("synthetic context"));
    client.add_event_handler_context(7u64);

    let requests = server
        .received_requests()
        .await
        .expect("mock server request recording must be enabled");
    assert!(
        requests.is_empty(),
        "context registration contacted the server"
    );
}
