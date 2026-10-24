use persephone_contracts::{
    AccountState, AccountStatus, AppStatus, RoomList, SignInError, StoreState,
    generated_typescript, sample_room_summary, sample_timeline_page,
};

#[test]
fn checked_in_types_match_generator_without_mutating_files() {
    assert_eq!(
        generated_typescript(),
        include_str!("../generated/contracts.ts")
    );
}

#[test]
fn serde_serialized_fixtures_match_checked_in_json() {
    let summary = sample_room_summary();
    assert_eq!(
        serde_json::to_value(&summary).unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!("../fixtures/room-summary.json"))
            .unwrap()
    );

    let page = sample_timeline_page();
    assert_eq!(
        serde_json::to_value(&page).unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!("../fixtures/timeline-page.json"))
            .unwrap()
    );
}

#[test]
fn serde_rejects_unknown_fields_and_variants() {
    let unknown_field = r#"{"roomId":"!r:s","displayName":null,"unreadCount":1,"encryption":"encrypted","secret":"x"}"#;
    assert!(serde_json::from_str::<persephone_contracts::RoomSummary>(unknown_field).is_err());

    let unknown_variant = r#"{"kind":"future_variant","value":1}"#;
    assert!(serde_json::from_str::<persephone_contracts::TimelineItem>(unknown_variant).is_err());

    let unknown_message_field = r#"{"kind":"message","event_id":"$e","sender":"@a:s","body":"hi","origin_server_ts":null,"extra":true}"#;
    assert!(
        serde_json::from_str::<persephone_contracts::TimelineItem>(unknown_message_field).is_err()
    );

    let unknown_gap_field = r#"{"kind":"gap","next_cursor":null,"extra":true}"#;
    assert!(serde_json::from_str::<persephone_contracts::TimelineItem>(unknown_gap_field).is_err());

    let unknown_nested_field = r#"{"roomId":"!r:s","items":[{"kind":"gap","next_cursor":null,"extra":true}],"nextCursor":null}"#;
    assert!(
        serde_json::from_str::<persephone_contracts::TimelinePage>(unknown_nested_field).is_err()
    );
}

#[test]
fn serde_enforces_u32_integer_range_for_json_count() {
    type Summary = persephone_contracts::RoomSummary;

    let valid_max =
        r#"{"roomId":"!r:s","displayName":null,"unreadCount":4294967295,"encryption":"encrypted"}"#;
    assert_eq!(
        serde_json::from_str::<Summary>(valid_max)
            .unwrap()
            .unread_count,
        u32::MAX
    );

    for invalid in [
        r#"{"roomId":"!r:s","displayName":null,"unreadCount":-1,"encryption":"encrypted"}"#,
        r#"{"roomId":"!r:s","displayName":null,"unreadCount":1.5,"encryption":"encrypted"}"#,
        r#"{"roomId":"!r:s","displayName":null,"unreadCount":4294967296,"encryption":"encrypted"}"#,
    ] {
        assert!(
            serde_json::from_str::<Summary>(invalid).is_err(),
            "{invalid}"
        );
    }
}

/// GIVEN a time WHEN it is read THEN only values JavaScript holds exactly are
/// accepted, so the page never shows a rounded time.
#[test]
fn serde_enforces_javascript_safe_range_for_times() {
    use persephone_contracts::{TimelineItem, UnixMillis};

    let message = |ts: &str| {
        format!(
            r#"{{"kind":"message","event_id":"$e","sender":"@a:s","body":"hi","origin_server_ts":{ts}}}"#
        )
    };
    let time = |item: TimelineItem| match item {
        TimelineItem::Message {
            origin_server_ts, ..
        } => origin_server_ts.map(UnixMillis::get),
        _ => unreachable!(),
    };
    assert_eq!(time(serde_json::from_str(&message("null")).unwrap()), None);
    assert_eq!(
        time(serde_json::from_str(&message("9007199254740991")).unwrap()),
        Some(UnixMillis::MAX)
    );
    for invalid in [
        "9007199254740992",
        "18446744073709551615",
        "-1",
        "1.5",
        "\"1\"",
    ] {
        assert!(
            serde_json::from_str::<TimelineItem>(&message(invalid)).is_err(),
            "{invalid}"
        );
    }
    // A missing time reads as none, never as a default time.
    assert_eq!(
        time(
            serde_json::from_str(
                r#"{"kind":"message","event_id":"$e","sender":"@a:s","body":"hi"}"#
            )
            .unwrap()
        ),
        None
    );
    assert_eq!(UnixMillis::new(UnixMillis::MAX + 1), None);
    assert_eq!(UnixMillis::saturating(u64::MAX).get(), UnixMillis::MAX);

    let pending = |queued_at: &str| {
        format!(
            r#"{{"localId":1,"roomId":"!r:s","kind":"text","body":"b","state":"queued","eventId":null,"failure":null,"queuedAt":{queued_at}}}"#
        )
    };
    assert!(serde_json::from_str::<persephone_contracts::PendingMessage>(&pending("0")).is_ok());
    assert!(
        serde_json::from_str::<persephone_contracts::PendingMessage>(&pending("9007199254740992"))
            .is_err()
    );
    assert!(
        serde_json::from_str::<persephone_contracts::PendingMessage>(&pending("null")).is_err()
    );
}

#[test]
fn app_status_serializes_camel_case_and_rejects_unknown_fields() {
    let status = AppStatus {
        version: "0.0.0".into(),
        milestone: "P1".into(),
        store: StoreState::KeyMissing,
        account: AccountStatus {
            state: AccountState::SignInFailed,
            user_id: None,
            error: Some(SignInError::InvalidCredentials),
        },
    };
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        serde_json::json!({
            "version": "0.0.0", "milestone": "P1", "store": "key_missing",
            "account": {"state": "sign_in_failed", "userId": null, "error": "invalid_credentials"}
        })
    );
    let account = r#""account":{"state":"signed_out","userId":null,"error":null}"#;
    for invalid in [
        format!(r#"{{"version":"0","milestone":"P1","store":"protected",{account},"extra":1}}"#),
        r#"{"version":"0","milestone":"P1","store":"protected"}"#.to_string(),
        format!(r#"{{"version":"0","milestone":"P1","store":"plaintext",{account}}}"#),
        r#"{"version":"0","milestone":"P1","store":"protected","account":{"state":"signed_in","userId":"@a:s","error":null,"accessToken":"x"}}"#.to_string(),
        r#"{"version":"0","milestone":"P1","store":"protected","account":{"state":"logged_in","userId":null,"error":null}}"#.to_string(),
    ] {
        assert!(
            serde_json::from_str::<AppStatus>(&invalid).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn room_list_rejects_unknown_fields() {
    assert!(serde_json::from_str::<RoomList>(r#"{"rooms":[],"truncated":false}"#).is_ok());
    assert!(
        serde_json::from_str::<RoomList>(r#"{"rooms":[],"truncated":false,"cursor":"x"}"#).is_err()
    );
}

#[test]
fn image_fixtures_match_checked_in_json() {
    use persephone_contracts::{sample_image_pick, sample_image_view, sample_pending_image};
    for (value, fixture) in [
        (
            serde_json::to_value(sample_image_view()).unwrap(),
            include_str!("../fixtures/image-view.json"),
        ),
        (
            serde_json::to_value(sample_image_pick()).unwrap(),
            include_str!("../fixtures/image-pick.json"),
        ),
        (
            serde_json::to_value(sample_pending_image()).unwrap(),
            include_str!("../fixtures/pending-image.json"),
        ),
    ] {
        assert_eq!(
            value,
            serde_json::from_str::<serde_json::Value>(fixture).unwrap()
        );
    }
    assert_eq!(
        serde_json::to_value(persephone_contracts::ImagePick::Cancelled {}).unwrap(),
        serde_json::json!({"kind": "cancelled"})
    );
}

/// GIVEN image DTOs WHEN read THEN unknown fields, tags and MIME types, a
/// missing kind, and sizes outside JavaScript-safe u32 are refused.
#[test]
fn image_contracts_reject_unknown_fields_and_out_of_range_values() {
    use persephone_contracts::{ImagePick, ImageView, PendingMessage, TimelineItem};
    let image = |extra: &str| {
        format!(
            r#"{{"kind":"image","event_id":"$e","sender":"@a:s","origin_server_ts":null,"name":"n","size":1,"width":2,"height":3,"encrypted":false{extra}}}"#
        )
    };
    assert!(serde_json::from_str::<TimelineItem>(&image("")).is_ok());
    for extra in [r#","url":"mxc://s/m""#, r#","thumbnail":null"#] {
        assert!(serde_json::from_str::<TimelineItem>(&image(extra)).is_err());
    }
    let sized = |size: &str| {
        format!(
            r#"{{"kind":"image","event_id":"$e","sender":"@a:s","origin_server_ts":null,"name":"n","size":{size},"width":null,"height":null,"encrypted":true}}"#
        )
    };
    assert!(serde_json::from_str::<TimelineItem>(&sized("null")).is_ok());
    assert!(serde_json::from_str::<TimelineItem>(&sized("4294967295")).is_ok());
    for invalid in ["4294967296", "-1", "1.5", "\"1\""] {
        assert!(
            serde_json::from_str::<TimelineItem>(&sized(invalid)).is_err(),
            "{invalid}"
        );
    }

    let view = r#"{"handle":"h","width":1,"height":1,"bands":1,"bandRows":1}"#;
    assert!(serde_json::from_str::<ImageView>(view).is_ok());
    for invalid in [
        r#"{"handle":"h","width":1,"height":1,"bands":1}"#,
        r#"{"handle":"h","width":1,"height":1,"bands":1,"bandRows":1,"path":"C:\\x"}"#,
        r#"{"handle":"h","width":-1,"height":1,"bands":1,"bandRows":1}"#,
    ] {
        // Each case is well-formed JSON, so it is the schema that refuses it.
        assert!(
            serde_json::from_str::<serde_json::Value>(invalid).is_ok(),
            "{invalid}"
        );
        let error = serde_json::from_str::<ImageView>(invalid).unwrap_err();
        assert!(error.is_data(), "{invalid}: {error}");
    }
    let with_path = r#"{"handle":"h","width":1,"height":1,"bands":1,"bandRows":1,"path":"C:\\x"}"#;
    let error = serde_json::from_str::<ImageView>(with_path).unwrap_err();
    assert!(
        error.to_string().contains("unknown field `path`"),
        "{error}"
    );

    let draft = |mime: &str, extra: &str| {
        format!(r#"{{"kind":"draft","preview":{view},"size":10,"mime":"{mime}"{extra}}}"#)
    };
    assert!(serde_json::from_str::<ImagePick>(&draft("image/png", "")).is_ok());
    for invalid in [
        draft("image/gif", ""),
        draft("png", ""),
        draft("image/jpeg", r#","filename":"x.jpg""#),
        r#"{"kind":"cancelled","path":"x"}"#.to_owned(),
        r#"{"kind":"chosen"}"#.to_owned(),
        r#"{"preview":null}"#.to_owned(),
    ] {
        assert!(
            serde_json::from_str::<ImagePick>(&invalid).is_err(),
            "{invalid}"
        );
    }
    assert!(serde_json::from_str::<ImagePick>(r#"{"kind":"cancelled"}"#).is_ok());

    let pending = |kind: &str| {
        format!(
            r#"{{"localId":1,"roomId":"!r:s","kind":{kind},"body":"","state":"queued","eventId":null,"failure":null,"queuedAt":0}}"#
        )
    };
    assert!(serde_json::from_str::<PendingMessage>(&pending(r#""image""#)).is_ok());
    assert!(serde_json::from_str::<PendingMessage>(&pending(r#""file""#)).is_err());
    assert!(serde_json::from_str::<PendingMessage>(&pending("null")).is_err());
}
