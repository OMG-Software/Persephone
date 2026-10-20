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

/// Walks a room of large messages through the real frame writer and reader:
/// every page must fit one control frame. Invalid arguments get error replies
/// and the session continues.
#[test]
fn given_large_history_then_every_page_fits_a_control_frame_and_bad_arguments_get_errors() {
    use persephone_contracts::TimelinePage;
    use persephone_core_protocol::ErrorCode;
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut input = Vec::new();
        send(
            &mut input,
            &Request {
                id: pages,
                call: Call::TimelinePage {
                    room_id: "!long:example.invalid".into(),
                    cursor: cursor.clone(),
                },
            },
        )
        .unwrap();
        let mut output = Vec::new();
        assert_eq!(serve(Cursor::new(input), &mut output), ServeEnd::UiClosed);
        assert!(
            output.len() <= 17 + 128 * 1024,
            "reply frame {} bytes",
            output.len()
        );
        let reply = replies(output).pop().expect("a page reply");
        let Outcome::Ok(value) = reply.outcome else {
            panic!("{reply:?}");
        };
        let page: TimelinePage = serde_json::from_value(value).unwrap();
        pages += 1;
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert!(
        pages >= 4,
        "60 messages of 8 KiB need several pages, got {pages}"
    );

    let mut input = Vec::new();
    for (id, room, cursor) in [
        (1, "!missing:example.invalid", None),
        (2, "!planning:example.invalid", Some("t1.0".to_string())),
    ] {
        send(
            &mut input,
            &Request {
                id,
                call: Call::TimelinePage {
                    room_id: room.into(),
                    cursor,
                },
            },
        )
        .unwrap();
    }
    input.extend(requests(&[3]));
    let mut output = Vec::new();
    assert_eq!(serve(Cursor::new(input), &mut output), ServeEnd::UiClosed);
    let outcomes: Vec<Outcome> = replies(output).into_iter().map(|r| r.outcome).collect();
    assert_eq!(outcomes[0], Outcome::Error(ErrorCode::NotFound));
    assert_eq!(outcomes[1], Outcome::Error(ErrorCode::InvalidArgument));
    assert!(matches!(outcomes[2], Outcome::Ok(_)), "session continued");
}
