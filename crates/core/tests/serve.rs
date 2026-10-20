//! The core's request loop, driven over in-memory streams.

use persephone_contracts::AppStatus;
use persephone_core::{MILESTONE, ServeEnd, serve};
use persephone_core_protocol::{Call, Outcome, Reply, Request, receive, send};
use persephone_media_protocol::{FrameHeader, FrameKind, ResourceBudgets};
use std::io::Cursor;

fn requests(ids: &[u32]) -> Vec<u8> {
    let mut wire = Vec::new();
    for &id in ids {
        send(
            &mut wire,
            &Request {
                id,
                call: Call::AppStatus {},
            },
        )
        .unwrap();
    }
    wire
}

fn replies(wire: Vec<u8>) -> Vec<Reply> {
    let budgets = ResourceBudgets::default();
    let mut reader = Cursor::new(wire);
    std::iter::from_fn(|| receive::<_, Reply>(&mut reader, &budgets).ok()).collect()
}

fn raw_frame(kind: FrameKind, body: &[u8]) -> Vec<u8> {
    let mut wire = FrameHeader {
        kind,
        length: body.len() as u64,
        width: 0,
        height: 0,
    }
    .encode()
    .to_vec();
    wire.extend_from_slice(body);
    wire
}

#[test]
fn given_requests_then_each_gets_a_matching_typed_reply_until_the_ui_closes() {
    let mut output = Vec::new();
    assert_eq!(
        serve(Cursor::new(requests(&[1, 2, 9])), &mut output),
        ServeEnd::UiClosed
    );
    let replies = replies(output);
    assert_eq!(replies.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 2, 9]);
    for reply in replies {
        let Outcome::Ok(value) = reply.outcome else {
            panic!("app_status failed: {reply:?}");
        };
        let status: AppStatus = serde_json::from_value(value).unwrap();
        assert_eq!(status.milestone, MILESTONE);
        assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    }
}

/// Anything a correct UI never sends ends the session: earlier requests are
/// answered, the bad one and everything after it are not.
#[test]
fn given_a_protocol_violation_then_the_core_stops_serving_without_replying_to_it() {
    for bad in [
        raw_frame(
            FrameKind::Control,
            br#"{"id":5,"call":{"command":"export_keys"}}"#,
        ),
        raw_frame(FrameKind::Control, b"\xff\xfe not json"),
        raw_frame(FrameKind::EncodedImage, b"{}"),
        // A header announcing 64 MiB: refused from the header alone.
        FrameHeader {
            kind: FrameKind::Control,
            length: 64 * 1024 * 1024,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec(),
        vec![99; 17],
    ] {
        let mut input = requests(&[1]);
        input.extend(bad);
        input.extend(requests(&[2]));
        let mut output = Vec::new();
        assert_eq!(
            serve(Cursor::new(input), &mut output),
            ServeEnd::ProtocolViolation
        );
        let replies = replies(output);
        assert_eq!(replies.iter().map(|r| r.id).collect::<Vec<_>>(), [1]);
    }
}

#[test]
fn given_truncated_frame_then_it_counts_as_the_ui_closing() {
    let mut input = requests(&[1]);
    let partial = requests(&[2]);
    input.extend(&partial[..partial.len() - 3]);
    let mut output = Vec::new();
    assert_eq!(serve(Cursor::new(input), &mut output), ServeEnd::UiClosed);
    assert_eq!(replies(output).len(), 1);
}
