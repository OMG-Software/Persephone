use persephone_contracts::{
    AccountChange, AccountSettings, AccountState, AccountStatus, AppStatus, CHAT_TEXT_PERCENT_MAX,
    ConversationStart, DeviceList, DisplayName, INTERFACE_TEXT_PERCENT_MAX, InviteList, JoinedRoom,
    NotificationLevel, NotificationSettings, PresenceStatus, ProfileList, PublicRoomPage,
    RoomCreation, RoomInvite, RoomList, SignInError, StoreState, TEXT_PERCENT_MIN, Theme,
    ThreePidList, TimelineEvents, UserSearch, generated_typescript, sample_room_summary,
    sample_timeline_page,
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

    let state = r#"{"kind":"state","event_id":"$e","sender":"@a:s","origin_server_ts":null,"change":{"type":"member","target":"@a:s","membership":"joined"}}"#;
    assert!(serde_json::from_str::<persephone_contracts::TimelineItem>(state).is_ok());
    for invalid in [
        r#"{"kind":"state","event_id":"$e","sender":"@a:s","origin_server_ts":null,"change":{"type":"member","target":"@a:s","membership":"joined","extra":1}}"#,
        r#"{"kind":"state","event_id":"$e","sender":"@a:s","origin_server_ts":null,"change":{"type":"member","target":"@a:s","membership":"teleported"}}"#,
        r#"{"kind":"state","event_id":"$e","sender":"@a:s","origin_server_ts":null,"change":{"type":"weather"}}"#,
        r#"{"kind":"state","event_id":"$e","sender":"@a:s","origin_server_ts":null,"change":{"type":"encryption","algorithm":"none"}}"#,
        r#"{"kind":"state","event_id":"$e","sender":"@a:s","origin_server_ts":null}"#,
    ] {
        assert!(
            serde_json::from_str::<persephone_contracts::TimelineItem>(invalid).is_err(),
            "{invalid}"
        );
    }

    // GIVEN an other event WHEN read THEN a known kind with its type and
    // flags reads, and an unknown kind, a missing type or flag, an extra
    // field or the removed `unsupported` notice is refused.
    let other = r#"{"kind":"other","event_id":"$e","sender":"@a:s","origin_server_ts":null,"event":"verification_request","event_type":"m.key.verification.request","state":false,"deleted":false}"#;
    assert!(serde_json::from_str::<persephone_contracts::TimelineItem>(other).is_ok());
    for invalid in [
        r#"{"kind":"other","event_id":"$e","sender":"@a:s","origin_server_ts":null,"event":"teleport","event_type":"x","state":false,"deleted":false}"#,
        r#"{"kind":"other","event_id":"$e","sender":"@a:s","origin_server_ts":null,"event":"event"}"#,
        r#"{"kind":"other","event_id":"$e","sender":"@a:s","origin_server_ts":null,"event":"event","event_type":"x","state":false,"deleted":false,"content":{}}"#,
        r#"{"kind":"other","event_id":"$e","sender":"@a:s","origin_server_ts":null,"event":"event","event_type":"x"}"#,
        r#"{"kind":"other","event_id":"$e","sender":"@a:s","origin_server_ts":null,"event":"event","event_type":"x","state":false}"#,
        r#"{"kind":"notice","event_id":"$e","sender":"@a:s","origin_server_ts":null,"notice":"unsupported"}"#,
    ] {
        assert!(
            serde_json::from_str::<persephone_contracts::TimelineItem>(invalid).is_err(),
            "{invalid}"
        );
    }

    let unknown_nested_field = r#"{"roomId":"!r:s","items":[{"kind":"gap","next_cursor":null,"extra":true}],"nextCursor":null}"#;
    assert!(
        serde_json::from_str::<persephone_contracts::TimelinePage>(unknown_nested_field).is_err()
    );

    // A name entry without a display name reads as having none; a name that
    // is not text, or an extra field, is refused.
    type Name = persephone_contracts::SenderName;
    let none: Name = serde_json::from_str(r#"{"userId":"@a:s","displayName":null}"#).unwrap();
    assert_eq!(none.display_name, None);
    for invalid in [
        r#"{"displayName":"A"}"#,
        r#"{"userId":"@a:s","displayName":7}"#,
        r#"{"userId":"@a:s","displayName":null,"avatarUrl":"mxc://s/m"}"#,
    ] {
        assert!(serde_json::from_str::<Name>(invalid).is_err(), "{invalid}");
    }
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
        revision: "0123456789ab".into(),
        milestone: "P1".into(),
        store: StoreState::KeyMissing,
        account: AccountStatus {
            state: AccountState::SignInFailed,
            user_id: None,
            error: Some(SignInError::InvalidCredentials),
            presence_refused: false,
        },
    };
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        serde_json::json!({
            "version": "0.0.0", "revision": "0123456789ab", "milestone": "P1", "store": "key_missing",
            "account": {"state": "sign_in_failed", "userId": null, "error": "invalid_credentials", "presenceRefused": false}
        })
    );
    // A refused presence is a flag only: no server text crosses (issue 86).
    let refused: AccountStatus = serde_json::from_str(
        r#"{"state":"signed_in","userId":"@a:s","error":null,"presenceRefused":true}"#,
    )
    .unwrap();
    assert!(refused.presence_refused);
    let account =
        r#""account":{"state":"signed_out","userId":null,"error":null,"presenceRefused":false}"#;
    for invalid in [
        format!(r#"{{"version":"0","revision":"unknown","milestone":"P1","store":"protected",{account},"extra":1}}"#),
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected"}"#.to_string(),
        format!(r#"{{"version":"0","milestone":"P1","store":"protected",{account}}}"#),
        format!(r#"{{"version":"0","revision":"unknown","milestone":"P1","store":"plaintext",{account}}}"#),
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected","account":{"state":"signed_in","userId":"@a:s","error":null,"presenceRefused":false,"accessToken":"x"}}"#.to_string(),
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected","account":{"state":"logged_in","userId":null,"error":null,"presenceRefused":false}}"#.to_string(),
        // The flag is required, and is a flag: no reason text.
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected","account":{"state":"signed_out","userId":null,"error":null}}"#.to_string(),
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected","account":{"state":"signed_in","userId":"@a:s","error":null,"presenceRefused":"Presence is disabled"}}"#.to_string(),
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
    use persephone_contracts::{
        sample_image_pick, sample_image_view, sample_pending_image, sample_room_picture,
    };
    for (value, fixture) in [
        (
            serde_json::to_value(sample_room_picture()).unwrap(),
            include_str!("../fixtures/room-picture.json"),
        ),
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

/// GIVEN a room picture reply or a room summary WHEN read THEN the picture is
/// named only by its ID: a URL, a missing image or an unknown field is refused.
#[test]
fn room_picture_contracts_carry_no_url() {
    use persephone_contracts::{RoomPicture, RoomSummary};
    let image = r#"{"handle":"h","width":96,"height":96,"bands":1,"bandRows":96}"#;
    let picture = format!(r#"{{"pictureId":"0123456789abcdef","image":{image}}}"#);
    assert!(serde_json::from_str::<RoomPicture>(&picture).is_ok());
    for invalid in [
        format!(r#"{{"pictureId":"0123456789abcdef","image":{image},"url":"mxc://s/m"}}"#),
        format!(r#"{{"image":{image}}}"#),
        r#"{"pictureId":"0123456789abcdef"}"#.to_owned(),
        format!(r#"{{"pictureId":null,"image":{image}}}"#),
    ] {
        assert!(
            serde_json::from_str::<RoomPicture>(&invalid).is_err(),
            "{invalid}"
        );
    }
    let summary = |extra: &str| {
        format!(
            r#"{{"roomId":"!r:s","displayName":null,"unreadCount":0,"encryption":"unknown","contact":null{extra}}}"#
        )
    };
    let without = serde_json::from_str::<RoomSummary>(&summary(r#","pictureId":null"#)).unwrap();
    assert_eq!(without.picture_id, None);
    assert!(serde_json::from_str::<RoomSummary>(&summary(r#","pictureUrl":"mxc://s/m""#)).is_err());
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
    assert!(serde_json::from_str::<PendingMessage>(&pending(r#""file""#)).is_ok());
    assert!(serde_json::from_str::<PendingMessage>(&pending(r#""video""#)).is_err());
    assert!(serde_json::from_str::<PendingMessage>(&pending("null")).is_err());
}

/// GIVEN conversation replies WHEN read THEN only the documented outcomes and
/// fields pass: no unencrypted outcome, no avatar URL, no extra invite data.
#[test]
fn conversation_replies_keep_their_documented_shape() {
    assert_eq!(
        serde_json::to_value(ConversationStart::Created {
            room_id: "!r:s".into()
        })
        .unwrap(),
        serde_json::json!({"kind": "created", "roomId": "!r:s"})
    );
    assert_eq!(
        serde_json::to_value(ConversationStart::NoIdentity {}).unwrap(),
        serde_json::json!({"kind": "no_identity"})
    );
    for valid in [
        r#"{"kind":"existing","roomId":"!r:s"}"#,
        r#"{"kind":"created","roomId":"!r:s"}"#,
        r#"{"kind":"no_identity"}"#,
        r#"{"kind":"refused"}"#,
    ] {
        assert!(
            serde_json::from_str::<ConversationStart>(valid).is_ok(),
            "{valid}"
        );
    }
    for invalid in [
        r#"{"kind":"created"}"#,
        r#"{"kind":"created","room_id":"!r:s"}"#,
        r#"{"kind":"created","roomId":"!r:s","encrypted":false}"#,
        r#"{"kind":"created_unencrypted","roomId":"!r:s"}"#,
        r#"{"kind":"no_identity","userId":"@b:s"}"#,
    ] {
        assert!(
            serde_json::from_str::<ConversationStart>(invalid).is_err(),
            "{invalid}"
        );
    }

    assert!(
        serde_json::from_str::<UserSearch>(
            r#"{"users":[{"userId":"@b:s","displayName":null}],"truncated":false}"#
        )
        .is_ok()
    );
    assert!(serde_json::from_str::<UserSearch>(
        r#"{"users":[{"userId":"@b:s","displayName":null,"avatarUrl":"mxc://s/m"}],"truncated":false}"#
    )
    .is_err());

    // A directory page carries no avatar: the page fetches no remote images.
    let room = r##"{"roomId":"!r:s","name":"Rust","alias":"#rust:s","topic":null,"members":3,"worldReadable":true,"guestsCanJoin":false,"joinable":true,"joined":false}"##;
    assert!(
        serde_json::from_str::<PublicRoomPage>(&format!(
            r#"{{"rooms":[{room}],"next":"p30","total":120}}"#
        ))
        .is_ok()
    );
    for invalid in [
        r#"{"rooms":[],"next":null,"total":null,"prev":"p0"}"#.to_owned(),
        format!(
            r#"{{"rooms":[{}],"next":null,"total":null}}"#,
            room.replace(
                r#""joined":false"#,
                r#""joined":false,"avatarUrl":"mxc://s/m""#
            )
        ),
    ] {
        assert!(
            serde_json::from_str::<PublicRoomPage>(&invalid).is_err(),
            "{invalid}"
        );
    }
    assert!(serde_json::from_str::<JoinedRoom>(r#"{"roomId":"!r:s"}"#).is_ok());
    assert!(serde_json::from_str::<JoinedRoom>(r#"{"roomId":"!r:s","via":"s"}"#).is_err());

    // A direct invitation, and one to a named room over federation whose
    // inviter the invitation did not say.
    for valid in [
        r#"{"invites":[{"roomId":"!r:s","roomName":null,"direct":true,"inviter":"@b:s","inviterName":null}],"truncated":false}"#,
        r#"{"invites":[{"roomId":"!r:t","roomName":"Plans","direct":false,"inviter":null,"inviterName":null}],"truncated":false}"#,
    ] {
        assert!(serde_json::from_str::<InviteList>(valid).is_ok(), "{valid}");
    }
    for invalid in [
        r#"{"invites":[{"roomId":"!r:s","roomName":null,"direct":true,"inviter":"@b:s","inviterName":null,"reason":"hi"}],"truncated":false}"#,
        r#"{"invites":[{"roomId":"!r:s","inviter":"@b:s","inviterName":null}],"truncated":false}"#,
        r#"{"invites":[{"roomId":"!r:s","roomName":null,"inviter":null,"inviterName":null}],"truncated":false}"#,
        r#"{"invites":[]}"#,
    ] {
        assert!(
            serde_json::from_str::<InviteList>(invalid).is_err(),
            "{invalid}"
        );
    }
}

/// GIVEN the room, account and profile replies WHEN read THEN the documented
/// shapes pass, and extra fields (an IP address, a path, an unencrypted room)
/// or missing ones are refused.
#[test]
fn room_account_and_profile_replies_keep_their_documented_shape() {
    for valid in [
        r#"{"kind":"created","roomId":"!r:s"}"#,
        r#"{"kind":"no_identity","userId":"@b:s"}"#,
        r#"{"kind":"refused"}"#,
    ] {
        assert!(
            serde_json::from_str::<RoomCreation>(valid).is_ok(),
            "{valid}"
        );
    }
    for invalid in [
        r#"{"kind":"created","roomId":"!r:s","encrypted":false}"#,
        r#"{"kind":"no_identity"}"#,
        r#"{"kind":"existing","roomId":"!r:s"}"#,
    ] {
        assert!(
            serde_json::from_str::<RoomCreation>(invalid).is_err(),
            "{invalid}"
        );
    }
    for valid in [
        r#"{"kind":"invited"}"#,
        r#"{"kind":"no_identity"}"#,
        r#"{"kind":"refused"}"#,
    ] {
        assert!(serde_json::from_str::<RoomInvite>(valid).is_ok(), "{valid}");
    }
    assert!(serde_json::from_str::<RoomInvite>(r#"{"kind":"invited","userId":"@b:s"}"#).is_err());

    assert!(serde_json::from_str::<DisplayName>(r#"{"displayName":null}"#).is_ok());
    assert!(
        serde_json::from_str::<DisplayName>(r#"{"displayName":"A","avatarUrl":"mxc://s/m"}"#)
            .is_err()
    );

    assert!(serde_json::from_str::<DeviceList>(
        r#"{"devices":[{"deviceId":"D","displayName":null,"lastSeen":1,"current":true,"verified":false}],"truncated":false}"#
    )
    .is_ok());
    for invalid in [
        r#"{"devices":[{"deviceId":"D","displayName":null,"lastSeen":1,"current":true,"verified":false,"lastSeenIp":"192.0.2.1"}],"truncated":false}"#,
        r#"{"devices":[{"deviceId":"D","displayName":null,"lastSeen":9007199254740992,"current":true,"verified":false}],"truncated":false}"#,
        r#"{"devices":[{"deviceId":"D","displayName":null,"lastSeen":1,"verified":false}],"truncated":false}"#,
    ] {
        assert!(
            serde_json::from_str::<DeviceList>(invalid).is_err(),
            "{invalid}"
        );
    }

    let defaults = AccountSettings::default();
    assert!(defaults.enter_sends && !defaults.desktop_notifications);
    assert!(!defaults.send_typing && !defaults.send_read_receipts);
    assert_eq!(defaults.idle_lock_minutes, 0);
    assert!(defaults.notification_names);
    assert!(
        !AccountSettings {
            idle_lock_minutes: persephone_contracts::IDLE_LOCK_MINUTES_MAX + 1,
            ..defaults
        }
        .within_bounds()
    );
    assert_eq!(
        (defaults.chat_text_percent, defaults.interface_text_percent),
        (100, 100)
    );
    assert_eq!(
        serde_json::to_string(&AccountSettings {
            enter_sends: false,
            ..defaults
        })
        .unwrap(),
        r#"{"enterSends":false,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true,"loadRoomPictures":false,"captureProtection":true,"timelineEvents":{"joins":true,"leaves":true,"invitations":true,"removals":true,"joinRequests":true,"displayNames":true,"profilePictures":true,"roomName":true,"topic":true,"roomPicture":true,"encryption":true,"joinRule":true,"historyVisibility":true,"permissions":true,"addresses":true,"pinnedMessages":true,"guestAccess":true,"serverAccess":true,"creation":true,"upgrades":true,"verification":true,"calls":true,"callSignalling":true,"polls":true,"stickers":true,"locations":true,"spaces":true,"widgets":true,"otherEvents":true,"deletedMessages":true},"theme":"orange","sharePresence":true,"presence":"online"}"#
    );
    // Every kind of room change shows by default.
    assert_eq!(defaults.timeline_events, TimelineEvents::default());
    assert!(defaults.timeline_events.leaves && defaults.timeline_events.room_picture);
    // GIVEN each theme WHEN it crosses THEN it keeps its snake_case name,
    // and grey and orange is the default.
    assert_eq!(defaults.theme, Theme::Orange);
    for (theme, name) in [
        (Theme::Orange, r#""orange""#),
        (Theme::Red, r#""red""#),
        (Theme::Blue, r#""blue""#),
    ] {
        assert_eq!(serde_json::to_string(&theme).unwrap(), name);
        assert_eq!(serde_json::from_str::<Theme>(name).unwrap(), theme);
    }
    // GIVEN settings from the page WHEN the theme is missing, unknown, not
    // a name, or in another case THEN the wire form refuses them: only the
    // stored form is lenient.
    let complete = serde_json::to_string(&defaults).unwrap();
    let without_theme = complete.replace(r#","theme":"orange""#, "");
    assert_ne!(without_theme, complete);
    for theme in [
        r#""green""#,
        "null",
        "1",
        r#""Orange""#,
        r#"{"orange":true}"#,
        r#"{"red":null}"#,
    ] {
        let sent = complete.replace(r#""theme":"orange""#, &format!(r#""theme":{theme}"#));
        assert!(
            serde_json::from_str::<AccountSettings>(&sent).is_err(),
            "{sent}"
        );
    }
    assert!(serde_json::from_str::<AccountSettings>(&without_theme).is_err());
    // GIVEN settings from the page WHEN presence is shared as online by
    // default THEN a missing presence field, or a status that is not one
    // of the three names, is refused (ADR-0057).
    assert!(defaults.share_presence);
    assert_eq!(defaults.presence, PresenceStatus::Online);
    for (status, name) in [
        (PresenceStatus::Online, r#""online""#),
        (PresenceStatus::Unavailable, r#""unavailable""#),
        (PresenceStatus::Offline, r#""offline""#),
    ] {
        assert_eq!(serde_json::to_string(&status).unwrap(), name);
        assert_eq!(
            serde_json::from_str::<PresenceStatus>(name).unwrap(),
            status
        );
    }
    for sent in [
        complete.replace(r#","sharePresence":true"#, ""),
        complete.replace(r#","presence":"online""#, ""),
        complete.replace(r#""sharePresence":true"#, r#""sharePresence":"yes""#),
        complete.replace(r#""presence":"online""#, r#""presence":"busy""#),
        complete.replace(r#""presence":"online""#, r#""presence":{"online":null}"#),
    ] {
        assert_ne!(sent, complete);
        assert!(
            serde_json::from_str::<AccountSettings>(&sent).is_err(),
            "{sent}"
        );
    }
    for invalid in [
        r#"{}"#,
        r#"{"enterSends":false}"#,
        r#"{"enterSends":null,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false}"#,
        r#"{"enterSends":"true","chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false}"#,
        r#"{"enterSends":true,"chatTextPercent":-1,"interfaceTextPercent":100,"desktopNotifications":false}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true,"loadRoomPictures":false,"userId":"@a:s"}"#,
        // A page from before the room picture setting.
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true,"loadRoomPictures":"yes"}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":-5}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":"yes","sendReadReceipts":false}"#,
        // An older page without the timeline switches, one switch missing,
        // a switch that is not true or false, and a switch this build does
        // not know are each refused.
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true,"timelineEvents":{"joins":true}}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true,"timelineEvents":{"joins":true,"leaves":"no","invitations":true,"removals":true,"joinRequests":true,"displayNames":true,"profilePictures":true,"roomName":true,"topic":true,"roomPicture":true,"encryption":true,"joinRule":true,"historyVisibility":true,"permissions":true,"addresses":true,"pinnedMessages":true,"guestAccess":true,"serverAccess":true,"creation":true,"upgrades":true,"verification":true,"calls":true,"callSignalling":true,"polls":true,"stickers":true,"locations":true,"spaces":true,"widgets":true,"otherEvents":true,"deletedMessages":true}}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true,"timelineEvents":{"joins":true,"leaves":true,"invitations":true,"removals":true,"joinRequests":true,"displayNames":true,"profilePictures":true,"roomName":true,"topic":true,"roomPicture":true,"encryption":true,"joinRule":true,"historyVisibility":true,"permissions":true,"addresses":true,"pinnedMessages":true,"guestAccess":true,"serverAccess":true,"creation":true,"upgrades":true,"verification":true,"calls":true,"callSignalling":true,"polls":true,"stickers":true,"locations":true,"spaces":true,"widgets":true,"otherEvents":true,"deletedMessages":true,"futureKind":false}}"#,
    ] {
        assert!(
            serde_json::from_str::<AccountSettings>(invalid).is_err(),
            "{invalid}"
        );
    }

    assert!(
        serde_json::from_str::<ProfileList>(
            r#"{"profiles":[{"slot":1,"active":true}],"canAdd":true}"#
        )
        .is_ok()
    );
    for invalid in [
        r#"{"profiles":[{"slot":1,"active":true,"path":"C:\x"}],"canAdd":true}"#,
        r#"{"profiles":[{"slot":1,"active":true,"userId":"@a:s"}],"canAdd":true}"#,
        r#"{"profiles":[{"slot":-1,"active":true}],"canAdd":true}"#,
    ] {
        assert!(
            serde_json::from_str::<ProfileList>(invalid).is_err(),
            "{invalid}"
        );
    }
}

/// GIVEN the level key for joining calls WHEN read THEN only its exact
/// name is accepted, it is listed once, and a call refusal and end code have
/// their own names.
#[test]
fn the_call_join_permission_has_closed_names() {
    use persephone_contracts::{CallEnd, CallJoinError, LevelKey, RoomLevel};
    assert!(
        serde_json::from_str::<RoomLevel>(r#"{"key":"join_calls","level":0}"#)
            .is_ok_and(|level| level.key == LevelKey::JoinCalls)
    );
    for invalid in [
        r#"{"key":"join_call","level":0}"#,
        r#"{"key":"JoinCalls","level":0}"#,
        r#"{"key":"org.matrix.msc3401.call.member","level":0}"#,
        r#"{"key":"join_calls","level":9007199254740992}"#,
    ] {
        assert!(
            serde_json::from_str::<RoomLevel>(invalid).is_err(),
            "{invalid}"
        );
    }
    assert_eq!(
        LevelKey::ALL
            .iter()
            .filter(|key| **key == LevelKey::JoinCalls)
            .count(),
        1
    );
    assert_eq!(
        serde_json::to_string(&CallJoinError::NotPermitted).unwrap(),
        r#""not_permitted""#
    );
    assert_eq!(
        serde_json::to_string(&CallEnd::Alone).unwrap(),
        r#""alone""#
    );
}

/// GIVEN power levels WHEN read THEN only whole numbers JavaScript holds
/// exactly are accepted, in settings and in changes alike.
#[test]
fn power_levels_stay_within_javascript_exact_range() {
    use persephone_contracts::{PowerLevel, RoomChange, RoomLevel};
    assert_eq!(
        PowerLevel::new(PowerLevel::MAX).map(PowerLevel::get),
        Some((1 << 53) - 1)
    );
    assert!(PowerLevel::new(PowerLevel::MAX + 1).is_none());
    assert!(PowerLevel::new(PowerLevel::MIN - 1).is_none());
    assert!(
        serde_json::from_str::<RoomLevel>(r#"{"key":"ban","level":-9007199254740991}"#).is_ok()
    );
    for invalid in [
        r#"{"key":"ban","level":9007199254740992}"#,
        r#"{"key":"ban","level":-9007199254740992}"#,
        r#"{"key":"ban","level":50.5}"#,
        r#"{"key":"ban","level":"50"}"#,
        r#"{"key":"m.room.ban","level":50}"#,
        r#"{"key":"ban","level":50,"event":"m.room.ban"}"#,
    ] {
        assert!(
            serde_json::from_str::<RoomLevel>(invalid).is_err(),
            "{invalid}"
        );
    }
    assert!(
        serde_json::from_str::<RoomChange>(
            r#"{"kind":"member_level","userId":"@b:s","level":1e20}"#
        )
        .is_err()
    );
}

/// GIVEN room changes WHEN read THEN only the closed set of kinds and their
/// own fields is accepted; a required field may not be left out, even when it
/// may be null.
#[test]
fn room_changes_are_a_closed_set() {
    use persephone_contracts::RoomChange;
    for valid in [
        r#"{"kind":"name","name":""}"#,
        r#"{"kind":"encryption"}"#,
        r#"{"kind":"leave"}"#,
        r##"{"kind":"canonical_alias","alias":null,"altAliases":[]}"##,
        r#"{"kind":"notifications","mode":"mentions"}"#,
        r#"{"kind":"history_visibility","visibility":"world_readable"}"#,
    ] {
        assert!(serde_json::from_str::<RoomChange>(valid).is_ok(), "{valid}");
    }
    for invalid in [
        r#"{}"#,
        r#"{"kind":"encryption","enabled":false}"#,
        r#"{"kind":"leave","forget":true}"#,
        r#"{"kind":"canonical_alias","altAliases":[]}"#,
        r#"{"kind":"canonical_alias","alias":null,"alt_aliases":[]}"#,
        r#"{"kind":"kick","user_id":"@b:s"}"#,
        r#"{"kind":"state","type":"m.room.server_acl","content":{}}"#,
        r#"{"kind":"join_rule","rule":"open"}"#,
        r#"{"kind":"notifications","mode":"loud"}"#,
    ] {
        assert!(
            serde_json::from_str::<RoomChange>(invalid).is_err(),
            "{invalid}"
        );
    }
}

/// GIVEN text sizes WHEN checked THEN only sizes within each setting's
/// bounds pass; the interface stops lower than message text.
#[test]
fn given_text_sizes_then_only_bounded_ones_pass() {
    let with = |chat, interface| AccountSettings {
        chat_text_percent: chat,
        interface_text_percent: interface,
        ..AccountSettings::default()
    };
    for (chat, interface) in [
        (TEXT_PERCENT_MIN, TEXT_PERCENT_MIN),
        (CHAT_TEXT_PERCENT_MAX, INTERFACE_TEXT_PERCENT_MAX),
        (100, 100),
    ] {
        assert!(with(chat, interface).within_bounds(), "{chat} {interface}");
    }
    for (chat, interface) in [
        (TEXT_PERCENT_MIN - 1, 100),
        (CHAT_TEXT_PERCENT_MAX + 1, 100),
        (100, TEXT_PERCENT_MIN - 1),
        (100, INTERFACE_TEXT_PERCENT_MAX + 1),
        (0, 0),
        (u16::MAX, u16::MAX),
    ] {
        assert!(!with(chat, interface).within_bounds(), "{chat} {interface}");
    }
}

/// GIVEN account-change replies WHEN read THEN only the fixed codes and the
/// exact list shape are accepted.
#[test]
fn given_account_change_replies_then_only_fixed_shapes_are_accepted() {
    assert_eq!(
        serde_json::from_str::<AccountChange>(r#""token_sent""#).unwrap(),
        AccountChange::TokenSent
    );
    assert!(serde_json::from_str::<AccountChange>(r#""M_FORBIDDEN""#).is_err());
    assert_eq!(
        serde_json::from_str::<NotificationSettings>(r#"{"level":"mentions"}"#)
            .unwrap()
            .level,
        NotificationLevel::Mentions
    );
    assert!(serde_json::from_str::<NotificationSettings>(r#"{"level":"loud"}"#).is_err());
    assert!(
        serde_json::from_str::<ThreePidList>(
            r#"{"threePids":[{"medium":"email","address":"a@example.org"}],"truncated":false,"canChange":true,"pending":null}"#
        )
        .is_ok()
    );
    for invalid in [
        r#"{"threePids":[{"medium":"fax","address":"1"}],"truncated":false,"canChange":true,"pending":null}"#,
        r#"{"threePids":[{"medium":"email","address":"a@example.org","validatedAt":1}],"truncated":false,"canChange":true,"pending":null}"#,
    ] {
        assert!(
            serde_json::from_str::<ThreePidList>(invalid).is_err(),
            "{invalid}"
        );
    }
}

/// GIVEN the call device reply
/// WHEN it is serialized and read back
/// THEN it uses camelCase names, keeps the new codes, and rejects extra fields.
#[test]
fn call_devices_use_camel_case_and_reject_unknown_fields() {
    let devices = persephone_contracts::CallDevices {
        supported: true,
        recording: vec![persephone_contracts::CallDevice {
            id: "{0.0.1}.{a}".into(),
            name: "Headset".into(),
        }],
        playout: Vec::new(),
        cameras: vec![persephone_contracts::CallDevice {
            id: "usb#cam".into(),
            name: "Camera".into(),
        }],
        selected_recording: Some("{0.0.1}.{a}".into()),
        selected_playout: None,
        selected_camera: Some("usb#cam".into()),
        volume: 40,
        auto_level: false,
        mic_gain: 250,
        unreadable: false,
    };
    let json = serde_json::to_value(&devices).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "supported": true,
            "recording": [{"id": "{0.0.1}.{a}", "name": "Headset"}],
            "playout": [],
            "cameras": [{"id": "usb#cam", "name": "Camera"}],
            "selectedRecording": "{0.0.1}.{a}",
            "selectedPlayout": null,
            "selectedCamera": "usb#cam",
            "volume": 40,
            "autoLevel": false,
            "micGain": 250,
            "unreadable": false
        })
    );
    assert_eq!(
        serde_json::from_value::<persephone_contracts::CallDevices>(json.clone()).unwrap(),
        devices
    );
    let mut extra = json.clone();
    extra["path"] = "C:/x".into();
    assert!(serde_json::from_value::<persephone_contracts::CallDevices>(extra).is_err());
    let mut without = json.clone();
    without.as_object_mut().unwrap().remove("cameras");
    assert!(serde_json::from_value::<persephone_contracts::CallDevices>(without).is_err());
    let mut no_camera = json.clone();
    no_camera.as_object_mut().unwrap().remove("selectedCamera");
    assert!(
        serde_json::from_value::<persephone_contracts::CallDevices>(no_camera).is_err(),
        "a missing selectedCamera is rejected"
    );
    let mut null_camera = json.clone();
    null_camera["selectedCamera"] = serde_json::Value::Null;
    assert_eq!(
        serde_json::from_value::<persephone_contracts::CallDevices>(null_camera)
            .unwrap()
            .selected_camera,
        None
    );
    let tile = serde_json::json!({"tile": 0, "participant": null, "own": true, "stream": 3});
    assert_eq!(
        serde_json::from_value::<persephone_contracts::CallVideoTile>(tile.clone())
            .unwrap()
            .participant,
        None
    );
    let mut no_participant = tile.clone();
    no_participant
        .as_object_mut()
        .unwrap()
        .remove("participant");
    assert!(
        serde_json::from_value::<persephone_contracts::CallVideoTile>(no_participant).is_err(),
        "a missing participant is rejected"
    );
    for (stream, valid) in [
        (0_u64, true),
        (9_007_199_254_740_991, true),
        (9_007_199_254_740_992, false),
        (u64::MAX, false),
    ] {
        let mut numbered = tile.clone();
        numbered["stream"] = serde_json::json!(stream);
        assert_eq!(
            serde_json::from_value::<persephone_contracts::CallVideoTile>(numbered).is_ok(),
            valid,
            "stream {stream}"
        );
    }
    let mut no_stream = tile.clone();
    no_stream.as_object_mut().unwrap().remove("stream");
    assert!(
        serde_json::from_value::<persephone_contracts::CallVideoTile>(no_stream).is_err(),
        "a missing stream number is rejected"
    );
    assert_eq!(
        serde_json::to_value(persephone_contracts::CallEnd::DeviceNotFound).unwrap(),
        "device_not_found"
    );
    assert_eq!(
        serde_json::to_value(persephone_contracts::CallJoinError::DeviceSettingUnreadable).unwrap(),
        "device_setting_unreadable"
    );
}

/// GIVEN the profile passphrase setting WHEN a security status crosses the
/// wire THEN it carries `profilePassphrase`, off by default, the new codes
/// keep their snake_case names, and unknown fields stay refused.
#[test]
fn security_status_carries_the_profile_passphrase_setting() {
    use persephone_contracts::{SecurityError, SecurityOperation, SecurityStatus};
    let mut status = SecurityStatus::unknown();
    assert!(!status.profile_passphrase, "off by default");
    status.profile_passphrase = true;
    status.operation = SecurityOperation::WaitingForPassphrase;
    status.error = Some(SecurityError::PassphraseRejected);
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["profilePassphrase"], true);
    assert_eq!(json["operation"], "waiting_for_passphrase");
    assert_eq!(json["error"], "passphrase_rejected");
    assert_eq!(
        serde_json::to_value(SecurityError::ProfileUnavailable).unwrap(),
        "profile_unavailable"
    );
    assert_eq!(
        serde_json::from_value::<SecurityStatus>(json.clone()).unwrap(),
        status
    );
    let mut missing = json.clone();
    missing.as_object_mut().unwrap().remove("profilePassphrase");
    assert!(serde_json::from_value::<SecurityStatus>(missing).is_err());
    let mut extra = json;
    extra["passphrase"] = "x".into();
    assert!(serde_json::from_value::<SecurityStatus>(extra).is_err());
}

/// GIVEN the Linux vault reasons (ADR-0061) WHEN they cross to the page THEN
/// each is its own fixed snake_case code, and an unknown code is refused.
#[test]
fn linux_vault_reasons_are_fixed_store_codes() {
    for (state, code) in [
        (StoreState::NoSessionBus, "no_session_bus"),
        (StoreState::NoSecretService, "no_secret_service"),
        (StoreState::NoDefaultKeyring, "no_default_keyring"),
        (StoreState::NoDisplay, "no_display"),
        (StoreState::KeyringBusy, "keyring_busy"),
        (StoreState::Unsupported, "unsupported"),
    ] {
        assert_eq!(
            serde_json::to_value(state).unwrap(),
            serde_json::json!(code)
        );
        assert_eq!(
            serde_json::from_value::<StoreState>(serde_json::json!(code)).unwrap(),
            state
        );
    }
    assert!(serde_json::from_value::<StoreState>(serde_json::json!("no_keyring")).is_err());
}
