//! The core's request loop, driven over in-memory streams.

use persephone_contracts::AppStatus;
use persephone_core::{Core, MILESTONE, ServeEnd, views::Fixtures};
use persephone_core_protocol::{
    Call, CoreMessage, ErrorCode, Event, MAX_SUBSCRIPTIONS, Outcome, Reply, Request, receive, send,
};
use persephone_media_protocol::{FrameHeader, FrameKind, ResourceBudgets};
use std::io::Cursor;

mod common;

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

fn serve(input: Vec<u8>, output: &mut Vec<u8>) -> ServeEnd {
    persephone_core::serve(&Core::default(), Cursor::new(input), output)
}

fn messages(wire: Vec<u8>) -> Vec<CoreMessage> {
    let budgets = ResourceBudgets::default();
    let mut reader = Cursor::new(wire);
    std::iter::from_fn(|| receive::<_, CoreMessage>(&mut reader, &budgets).ok()).collect()
}

fn replies(wire: Vec<u8>) -> Vec<Reply> {
    messages(wire)
        .into_iter()
        .map(|message| match message {
            CoreMessage::Reply(reply) => reply,
            CoreMessage::Event(event) => panic!("unexpected event {event:?}"),
        })
        .collect()
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
    assert_eq!(serve(requests(&[1, 2, 9]), &mut output), ServeEnd::UiClosed);
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
        assert_eq!(serve(input, &mut output), ServeEnd::ProtocolViolation);
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
    assert_eq!(serve(input, &mut output), ServeEnd::UiClosed);
    assert_eq!(replies(output).len(), 1);
}

/// Walks a room of large messages through the real frame writer and reader:
/// every page must fit one control frame. Invalid arguments get error replies
/// and the session continues.
#[test]
fn given_large_history_then_every_page_fits_a_control_frame_and_bad_arguments_get_errors() {
    use persephone_contracts::TimelinePage;
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
        assert_eq!(serve(input, &mut output), ServeEnd::UiClosed);
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
    assert_eq!(serve(input, &mut output), ServeEnd::UiClosed);
    let outcomes: Vec<Outcome> = replies(output).into_iter().map(|r| r.outcome).collect();
    assert_eq!(outcomes[0], Outcome::Error(ErrorCode::NotFound));
    assert_eq!(outcomes[1], Outcome::Error(ErrorCode::InvalidArgument));
    assert!(matches!(outcomes[2], Outcome::Ok(_)), "session continued");
}

fn call(id: u32, call: Call) -> Request {
    Request { id, call }
}

fn subscribe(room: &str) -> Call {
    Call::Subscribe {
        room_id: room.into(),
    }
}

#[test]
fn given_subscriptions_then_bad_rooms_and_too_many_are_refused() {
    let core = Core::default();
    let ok = Outcome::Ok(serde_json::Value::Null);
    assert_eq!(
        core.handle(&call(1, subscribe("!planning:example.invalid")))
            .outcome,
        ok
    );
    // Subscribing twice is not an error and takes no extra slot.
    assert_eq!(
        core.handle(&call(2, subscribe("!planning:example.invalid")))
            .outcome,
        ok
    );
    assert_eq!(
        core.handle(&call(3, subscribe("!missing:example.invalid")))
            .outcome,
        Outcome::Error(ErrorCode::NotFound)
    );
    for bad in ["planning", &format!("!{}", "x".repeat(255))] {
        assert_eq!(
            core.handle(&call(4, subscribe(bad))).outcome,
            Outcome::Error(ErrorCode::InvalidArgument)
        );
    }

    let core = Core::new(Fixtures::default().with_empty_rooms(MAX_SUBSCRIPTIONS));
    for n in 0..MAX_SUBSCRIPTIONS {
        let room = format!("!empty-{n}:example.invalid");
        assert_eq!(core.handle(&call(5, subscribe(&room))).outcome, ok);
    }
    let room = "!planning:example.invalid";
    assert_eq!(
        core.handle(&call(6, subscribe(room))).outcome,
        Outcome::Error(ErrorCode::LimitExceeded)
    );
    // Freeing a slot makes room again.
    core.handle(&call(
        7,
        Call::Unsubscribe {
            room_id: "!empty-0:example.invalid".into(),
        },
    ));
    assert_eq!(core.handle(&call(8, subscribe(room))).outcome, ok);
}

#[test]
fn given_many_changes_before_the_ui_reads_then_one_notification_is_pending_per_room() {
    let core = Core::default();
    core.handle(&call(1, subscribe("!planning:example.invalid")));
    for n in 0..10_000 {
        assert!(core.post_message(
            "!planning:example.invalid",
            "@a:example.invalid",
            format!("m{n}")
        ));
    }
    // Unsubscribed rooms change silently.
    assert!(core.post_message("!bidi:example.invalid", "@a:example.invalid", "x".into()));
    assert!(!core.post_message("!missing:example.invalid", "@a:example.invalid", "x".into()));
    assert_eq!(core.pending_notifications(), 1);

    core.handle(&call(
        2,
        Call::Unsubscribe {
            room_id: "!planning:example.invalid".into(),
        },
    ));
    assert_eq!(
        core.pending_notifications(),
        0,
        "unsubscribing drops pending"
    );
}

#[test]
fn given_a_subscribed_room_changes_while_serving_then_the_ui_gets_an_event_and_the_new_message() {
    use persephone_contracts::{TimelineItem, TimelinePage};
    use std::io::{PipeReader, PipeWriter};

    let core = Core::default();
    let (from_ui, mut to_core): (PipeReader, PipeWriter) = std::io::pipe().unwrap();
    let (mut from_core, to_ui) = std::io::pipe().unwrap();
    let budgets = ResourceBudgets::default();
    std::thread::scope(|scope| {
        let server = scope.spawn(|| persephone_core::serve(&core, from_ui, to_ui));
        send(
            &mut to_core,
            &call(1, subscribe("!planning:example.invalid")),
        )
        .unwrap();
        let Ok(CoreMessage::Reply(reply)) = receive(&mut from_core, &budgets) else {
            panic!("subscribe reply");
        };
        assert_eq!(reply.outcome, Outcome::Ok(serde_json::Value::Null));

        core.post_message(
            "!planning:example.invalid",
            "@a:example.invalid",
            "fresh".into(),
        );
        let Ok(CoreMessage::Event(Event::RoomChanged { room_id })) =
            receive::<_, CoreMessage>(&mut from_core, &budgets)
        else {
            panic!("room changed event");
        };
        assert_eq!(room_id, "!planning:example.invalid");

        send(
            &mut to_core,
            &call(
                2,
                Call::TimelinePage {
                    room_id,
                    cursor: None,
                },
            ),
        )
        .unwrap();
        let Ok(CoreMessage::Reply(Reply {
            id: 2,
            outcome: Outcome::Ok(value),
        })) = receive(&mut from_core, &budgets)
        else {
            panic!("page reply");
        };
        let page: TimelinePage = serde_json::from_value(value).unwrap();
        let Some(TimelineItem::Message { body, event_id, .. }) = page.items.last() else {
            panic!("message");
        };
        assert_eq!((body.as_str(), event_id.as_str()), ("fresh", "$live-250"));

        drop(to_core);
        assert_eq!(server.join().unwrap(), ServeEnd::UiClosed);
    });
    assert!(
        !core.wait_running(std::time::Duration::from_secs(5)),
        "closed after serving"
    );
}

/// Over real pipes: locking tells the UI, drops subscriptions and refuses
/// protected views; unlocking tells the UI again and views return.
#[test]
fn given_a_profile_when_the_ui_locks_and_unlocks_then_views_follow_the_session() {
    use persephone_contracts::StoreState;
    use persephone_core::session::Session;
    use std::io::{PipeReader, PipeWriter};

    let dir = tempfile::TempDir::new().unwrap();
    let core = Core::default().with_session(Session::open(
        dir.path().to_path_buf(),
        Box::new(common::Tagged(1)),
    ));
    let (from_ui, mut to_core): (PipeReader, PipeWriter) = std::io::pipe().unwrap();
    let (mut from_core, to_ui) = std::io::pipe().unwrap();
    let budgets = ResourceBudgets::default();
    // Sends one call and returns its outcome and any events read before it.
    let mut ask = move |id: u32, call: Call| -> (Outcome, Vec<Event>) {
        send(&mut to_core, &self::call(id, call)).unwrap();
        let mut events = Vec::new();
        loop {
            match receive::<_, CoreMessage>(&mut from_core, &budgets).unwrap() {
                CoreMessage::Event(event) => events.push(event),
                CoreMessage::Reply(reply) => {
                    assert_eq!(reply.id, id);
                    return (reply.outcome, events);
                }
            }
        }
    };
    let state = |outcome: Outcome| match outcome {
        Outcome::Ok(value) => serde_json::from_value::<StoreState>(value).unwrap(),
        other => panic!("{other:?}"),
    };

    std::thread::scope(|scope| {
        let server = scope.spawn(|| persephone_core::serve(&core, from_ui, to_ui));
        assert!(matches!(ask(1, Call::RoomList {}).0, Outcome::Ok(_)));
        assert_eq!(
            ask(2, subscribe("!planning:example.invalid")).0,
            Outcome::Ok(serde_json::Value::Null)
        );

        let (outcome, mut events) = ask(3, Call::LockSession {});
        assert_eq!(state(outcome), StoreState::Locked);
        // The event may be written just before or just after the reply.
        events.extend(match ask(4, Call::AppStatus {}) {
            (Outcome::Ok(value), more) => {
                let status: AppStatus = serde_json::from_value(value).unwrap();
                assert_eq!(status.store, StoreState::Locked);
                more
            }
            other => panic!("{other:?}"),
        });
        assert_eq!(events, [Event::SessionChanged {}]);

        for (id, call) in [
            (5, Call::RoomList {}),
            (
                6,
                Call::TimelinePage {
                    room_id: "!planning:example.invalid".into(),
                    cursor: None,
                },
            ),
            (7, subscribe("!planning:example.invalid")),
        ] {
            assert_eq!(ask(id, call).0, Outcome::Error(ErrorCode::Locked));
        }
        // The subscription was dropped: a change queues nothing.
        core.post_message(
            "!planning:example.invalid",
            "@a:example.invalid",
            "x".into(),
        );
        assert_eq!(core.pending_notifications(), 0);

        let (outcome, mut events) = ask(8, Call::UnlockSession {});
        assert_eq!(state(outcome), StoreState::Protected);
        events.extend(ask(9, Call::AppStatus {}).1);
        assert_eq!(events, [Event::SessionChanged {}]);
        assert!(matches!(ask(10, Call::RoomList {}).0, Outcome::Ok(_)));

        // Closes the UI's ends of both pipes.
        drop(ask);
        assert_eq!(server.join().unwrap(), ServeEnd::UiClosed);
    });
    let (state, _) = core
        .session()
        .lock(std::time::Duration::from_secs(30), || {});
    assert_eq!(state, StoreState::Locked);
}
