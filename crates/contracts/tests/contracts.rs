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

    let unknown_message_field =
        r#"{"kind":"message","event_id":"$e","sender":"@a:s","body":"hi","extra":true}"#;
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
