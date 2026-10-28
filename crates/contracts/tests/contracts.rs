use persephone_contracts::{
    AccountChange, AccountSettings, AccountState, AccountStatus, AppStatus, CHAT_TEXT_PERCENT_MAX,
    ConversationStart, DeviceList, DisplayName, INTERFACE_TEXT_PERCENT_MAX, InviteList,
    NotificationLevel, NotificationSettings, ProfileList, RoomCreation, RoomInvite, RoomList,
    SignInError, StoreState, TEXT_PERCENT_MIN, ThreePidList, UserSearch, generated_typescript,
    sample_room_summary, sample_timeline_page,
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
        revision: "0123456789ab".into(),
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
            "version": "0.0.0", "revision": "0123456789ab", "milestone": "P1", "store": "key_missing",
            "account": {"state": "sign_in_failed", "userId": null, "error": "invalid_credentials"}
        })
    );
    let account = r#""account":{"state":"signed_out","userId":null,"error":null}"#;
    for invalid in [
        format!(r#"{{"version":"0","revision":"unknown","milestone":"P1","store":"protected",{account},"extra":1}}"#),
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected"}"#.to_string(),
        format!(r#"{{"version":"0","milestone":"P1","store":"protected",{account}}}"#),
        format!(r#"{{"version":"0","revision":"unknown","milestone":"P1","store":"plaintext",{account}}}"#),
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected","account":{"state":"signed_in","userId":"@a:s","error":null,"accessToken":"x"}}"#.to_string(),
        r#"{"version":"0","revision":"unknown","milestone":"P1","store":"protected","account":{"state":"logged_in","userId":null,"error":null}}"#.to_string(),
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
        r#"{"enterSends":false,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true}"#
    );
    for invalid in [
        r#"{}"#,
        r#"{"enterSends":false}"#,
        r#"{"enterSends":null,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false}"#,
        r#"{"enterSends":"true","chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false}"#,
        r#"{"enterSends":true,"chatTextPercent":-1,"interfaceTextPercent":100,"desktopNotifications":false}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":0,"notificationNames":true,"userId":"@a:s"}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"idleLockMinutes":-5}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false}"#,
        r#"{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":"yes","sendReadReceipts":false}"#,
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
