//! IPC permission tests against the shell's real configuration and compiled
//! capabilities, using Tauri's mock runtime (no window or web view).

use persephone_core_protocol::{Call, ErrorCode, Event};
use persephone_desktop::{
    BAND_PREFIX_BYTES, MAIN_WINDOW, Subscriptions, configure, context,
    core_link::{Band, CoreError, CoreLink, CoreTransport},
    create_main_window,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tauri::{
    ipc::{CallbackFn, InvokeBody},
    test::{INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder},
    webview::InvokeRequest,
};

/// Stands in for the core process. Counts calls so tests can show which
/// requests never reached it.
struct FakeCore {
    available: bool,
    calls: AtomicUsize,
}

impl FakeCore {
    fn new(available: bool) -> Arc<Self> {
        Arc::new(Self {
            available,
            calls: AtomicUsize::new(0),
        })
    }
}

impl CoreTransport for FakeCore {
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.available {
            return Err(CoreError::Unavailable);
        }
        match call {
            Call::AppStatus {} => Ok(
                serde_json::json!({"version": "9.9.9", "revision": "0123456789ab", "milestone": "P1", "store": "key_missing", "account": {"state": "signed_out", "userId": null, "error": null}}),
            ),
            Call::RoomList {} => Ok(serde_json::json!({
                "rooms": [{"roomId": "!r:example.invalid", "displayName": "<b>x</b>", "unreadCount": 2, "encryption": "encrypted"}],
                "truncated": false
            })),
            Call::Subscribe { room_id } => match room_id.as_str() {
                "!r:example.invalid" | "!s:example.invalid" => Ok(serde_json::Value::Null),
                "!full:example.invalid" => Err(CoreError::Core(ErrorCode::LimitExceeded)),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::Unsubscribe { .. } => Ok(serde_json::Value::Null),
            Call::LockSession {} => Ok(serde_json::json!("locked")),
            // A state outside the contract must not reach the page.
            Call::UnlockSession {} => Ok(serde_json::json!("plaintext")),
            Call::ResetProfile {} => Ok(serde_json::json!("unlocking")),
            Call::SignOut {} => Ok(serde_json::json!("restart_required")),
            Call::SecurityStatus {} => Ok(
                serde_json::json!({"identity": "none", "recovery": "disabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false}),
            ),
            Call::VerifyThisDevice {} => Ok(serde_json::json!({
                "identity": "unverified", "recovery": "enabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false,
                "verification": {"flowId": "f1", "phase": "waiting", "emoji": [], "otherDevice": null}
            })),
            Call::AcceptVerification { flow_id }
            | Call::ConfirmVerification { flow_id }
            | Call::CancelVerification { flow_id } => Ok(serde_json::json!({
                "identity": "unverified", "recovery": "enabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false,
                "verification": {"flowId": flow_id, "phase": "compare",
                    "emoji": [{"symbol": "🐶", "description": "Dog"}], "otherDevice": "<b>Laptop</b>"}
            })),
            Call::ContactStatus { room_id } => match room_id.as_str() {
                "!dm:example.invalid" => Ok(serde_json::json!({
                    "roomId": room_id, "userId": "<b>@bob</b>:example.invalid", "trust": "unverified"
                })),
                // A trust level outside the contract must not reach the page.
                "!odd:example.invalid" => Ok(serde_json::json!({
                    "roomId": room_id, "userId": "@bob:example.invalid", "trust": "trusted_anyway"
                })),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::IdentityChanges { room_id } => match room_id.as_str() {
                "!dm:example.invalid" => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false,
                    "changes": [{"userId": "@bob:example.invalid", "wasVerified": true}]
                })),
                // A reply with a field the contract does not have.
                _ => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false, "changes": [], "pinned": true
                })),
            },
            Call::LoadImage { event_id, .. } => match event_id.as_str() {
                "$image" => Ok(serde_json::json!({
                    "handle": HANDLE, "width": 3, "height": 5, "bands": 2, "bandRows": 4
                })),
                "$bad" => Err(CoreError::Core(ErrorCode::Rejected)),
                "$busy" => Err(CoreError::Core(ErrorCode::LimitExceeded)),
                // A view carrying a field outside the contract.
                _ => Ok(serde_json::json!({
                    "handle": HANDLE, "width": 1, "height": 1, "bands": 1, "bandRows": 1,
                    "url": "mxc://example.invalid/m"
                })),
            },
            // Answered with pixels, through `image_band`; the UI never sends
            // it as an ordinary call.
            Call::ImageBand { .. } => Err(CoreError::BadReply),
            Call::ReleaseImage { handle } | Call::DiscardImage { handle } => {
                if handle == HANDLE {
                    Ok(serde_json::Value::Null)
                } else {
                    Err(CoreError::Core(ErrorCode::NotFound))
                }
            }
            Call::PickImage { room_id } => match room_id.as_str() {
                "!r:example.invalid" => Ok(serde_json::json!({
                    "kind": "draft", "size": 48_213, "mime": "image/png",
                    "preview": {"handle": HANDLE, "width": 3, "height": 5, "bands": 2, "bandRows": 4}
                })),
                "!cancel:example.invalid" => Ok(serde_json::json!({"kind": "cancelled"})),
                // A draft carrying the file's path is refused by the contract.
                "!path:example.invalid" => Ok(serde_json::json!({
                    "kind": "draft", "size": 1, "mime": "image/png", "path": "C:\\Users\\a\\x.png",
                    "preview": {"handle": HANDLE, "width": 1, "height": 1, "bands": 1, "bandRows": 1}
                })),
                _ => Err(CoreError::Core(ErrorCode::Unavailable)),
            },
            Call::SendImage { room_id, handle } => {
                let mut queued = serde_json::json!({
                    "localId": 3, "roomId": room_id, "kind": "image", "body": "", "state": "queued",
                    "eventId": null, "failure": null, "queuedAt": 1_767_258_000_000_u64
                });
                match (room_id.as_str(), handle == HANDLE) {
                    ("!full:example.invalid", _) => Err(CoreError::Core(ErrorCode::LimitExceeded)),
                    (_, true) => Ok(queued),
                    // A reply carrying the draft handle is refused by the contract.
                    (_, false) => {
                        queued["handle"] = handle.into();
                        Ok(queued)
                    }
                }
            }
            Call::AcceptIdentity { .. } => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled", "operation": "waiting_for_confirmation",
                "error": null, "canSetUpRecovery": false, "verification": null
            })),
            Call::VerifyContact { .. } => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled", "operation": "working", "error": null, "canSetUpRecovery": false,
                "verification": null
            })),
            Call::SetUpRecovery {} => Ok(
                serde_json::json!({"identity": "verified", "recovery": "disabled", "operation": "working", "error": null, "verification": null, "canSetUpRecovery": false}),
            ),
            Call::SetUpEncryption {} => Ok(
                serde_json::json!({"identity": "none", "recovery": "disabled", "operation": "waiting_for_password", "error": null, "verification": null, "canSetUpRecovery": false}),
            ),
            // A reply carrying a secret is refused by the contract.
            Call::SendMessage { room_id, body, .. } | Call::EditMessage { room_id, body, .. } => {
                match body.as_str() {
                    "hello" => Ok(serde_json::json!({
                        "localId": 1, "roomId": room_id, "kind": "text", "body": body, "state": "queued",
                        "eventId": null, "failure": null, "queuedAt": 1_767_258_000_000_u64
                    })),
                    // A reply carrying the transaction ID is refused by the contract.
                    _ => Ok(serde_json::json!({
                        "localId": 2, "roomId": room_id, "kind": "text", "body": body, "state": "queued",
                        "eventId": null, "failure": null, "queuedAt": 1_767_258_000_000_u64, "transactionId": "t"
                    })),
                }
            }
            Call::PendingMessages { .. } => Ok(serde_json::json!([])),
            Call::RetryMessage { local_id } | Call::DiscardMessage { local_id } => match local_id {
                1 => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::AllowUnencrypted { .. } => Ok(serde_json::Value::Null),
            Call::RecoverWithKey {} => Ok(
                serde_json::json!({"identity": "unverified", "recovery": "enabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false, "recoveryKey": "x"}),
            ),
            Call::Register { username, .. } => match username.as_str() {
                "taken" => Ok(
                    serde_json::json!({"state": "sign_in_failed", "userId": null, "error": "username_taken"}),
                ),
                // A reason outside the contract is refused.
                "odd" => Ok(
                    serde_json::json!({"state": "sign_in_failed", "userId": null, "error": "password_was_hunter2"}),
                ),
                _ => Ok(serde_json::json!({"state": "signing_in", "userId": null, "error": null})),
            },
            Call::SignIn { homeserver } => match homeserver.as_str() {
                "https://matrix.example.org" => {
                    Ok(serde_json::json!({"state": "signing_in", "userId": null, "error": null}))
                }
                // A reply carrying a secret is refused by the contract.
                _ => Ok(
                    serde_json::json!({"state": "signed_in", "userId": "@a:s", "error": null, "accessToken": "x"}),
                ),
            },
            Call::TimelinePage { room_id, cursor } => match (room_id.as_str(), cursor.as_deref()) {
                ("!r:example.invalid", None) => Ok(serde_json::json!({
                    "roomId": room_id,
                    "items": [{"kind": "message", "event_id": "$1", "sender": "@a:example.invalid", "body": "hi", "origin_server_ts": 1_767_258_000_000_u64}],
                    "nextCursor": null
                })),
                // A reply with a field the contract does not have.
                ("!bad-reply:example.invalid", _) => Ok(serde_json::json!({
                    "roomId": room_id, "items": [], "nextCursor": null, "accessToken": "x"
                })),
                (_, Some(_)) => Err(CoreError::Core(ErrorCode::InvalidArgument)),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::InviteList {} => Ok(serde_json::json!({
                "invites": [
                    {"roomId": "!inv:example.invalid", "roomName": null, "direct": true, "inviter": "@bob:example.invalid", "inviterName": "<b>Bob</b>"},
                    {"roomId": "!fed:remote.invalid", "roomName": "Plans", "direct": false, "inviter": null, "inviterName": null}
                ],
                "truncated": false
            })),
            Call::MarkRead { .. } => Ok(serde_json::Value::Null),
            Call::RoomReaders { .. } => Ok(serde_json::json!({
                "readers": [{"userId": "@b:example.invalid", "trust": "verified",
                    "receiving": 2, "refused": 1, "own": false}],
                "truncated": false
            })),
            Call::RoomServers { .. } => Ok(serde_json::json!({
                "servers": [{"name": "example.invalid", "members": 2, "own": true}],
                "truncated": false
            })),
            Call::OpenLink { url } => Ok(serde_json::Value::Bool(url == "https://example.org/")),
            Call::RoomActivity { .. } => Ok(serde_json::json!({
                "typing": ["@b:example.invalid"], "readBy": [], "truncated": false
            })),
            Call::SetTyping { typing, .. } => match typing {
                true => Ok(serde_json::Value::Null),
                false => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::React { key, .. } => match key.as_str() {
                "👍" => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::DeleteMessage { event_id, .. } => match event_id.as_str() {
                "$mine" => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::AcceptInvite { room_id } | Call::DeclineInvite { room_id } => {
                match room_id.as_str() {
                    "!inv:example.invalid" => Ok(serde_json::Value::Null),
                    _ => Err(CoreError::Core(ErrorCode::NotFound)),
                }
            }
            Call::StartConversation { user_id } => match user_id.as_str() {
                "@bob:example.invalid" => {
                    Ok(serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"}))
                }
                "@nokeys:example.invalid" => Ok(serde_json::json!({"kind": "no_identity"})),
                // An outcome outside the contract must not reach the page.
                _ => Ok(
                    serde_json::json!({"kind": "created_unencrypted", "roomId": "!x:example.invalid"}),
                ),
            },
            Call::SearchUsers { term } => match term.as_str() {
                "bob" => Ok(serde_json::json!({
                    "users": [{"userId": "@bob:example.invalid", "displayName": "Bob"}],
                    "truncated": false
                })),
                // A match carrying a field the contract does not have.
                _ => Ok(serde_json::json!({
                    "users": [{"userId": "@a:example.invalid", "displayName": null, "avatarUrl": "mxc://x/y"}],
                    "truncated": false
                })),
            },
            Call::CreateRoom { name, invite } => match (name.as_str(), invite.as_slice()) {
                ("Plans", [user]) if user == "@bob:example.invalid" => {
                    Ok(serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"}))
                }
                ("Plans", [user]) => Ok(serde_json::json!({"kind": "no_identity", "userId": user})),
                // An outcome outside the contract must not reach the page.
                _ => {
                    Ok(serde_json::json!({"kind": "created", "roomId": "!x:s", "encrypted": false}))
                }
            },
            Call::InviteUser { user_id, .. } => match user_id.as_str() {
                "@bob:example.invalid" => Ok(serde_json::json!({"kind": "invited"})),
                _ => Ok(serde_json::json!({"kind": "refused"})),
            },
            Call::DisplayName {} => Ok(serde_json::json!({"displayName": "<b>Al</b>"})),
            Call::SetDisplayName { name } => match name.as_str() {
                "" => Ok(serde_json::json!({"displayName": null})),
                "Al" => Ok(serde_json::json!({"displayName": "Al"})),
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::AccountSettings {} => Ok(serde_json::json!({
                "enterSends": false, "chatTextPercent": 120, "interfaceTextPercent": 100,
                "desktopNotifications": true, "sendTyping": false, "sendReadReceipts": false,
                "idleLockMinutes": 0, "notificationNames": true
            })),
            Call::SetAccountSettings { settings } => match settings.enter_sends {
                true => Ok(serde_json::Value::Null),
                false => Err(CoreError::Core(ErrorCode::Locked)),
            },
            Call::DiscoverHomeserver { server_name } => match server_name.as_str() {
                "example.invalid" => Ok(serde_json::json!({
                    "homeserver": "https://matrix.example.invalid/", "fromWellKnown": true
                })),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::ProxySetting {} => Ok(serde_json::json!({"proxy": "127.0.0.1:9050"})),
            Call::SetProxySetting { proxy } => match proxy.as_deref() {
                None | Some("127.0.0.1:9050") => Ok(serde_json::Value::Null),
                Some(_) => Err(CoreError::Core(ErrorCode::InvalidArgument)),
            },
            Call::NewSessions {} => Ok(serde_json::json!({
                "sessions": [{"deviceId": "NEWDEV", "displayName": null}], "truncated": false
            })),
            Call::AcknowledgeSessions { .. } => Ok(serde_json::Value::Null),
            Call::RemoveDevice { device_id } => match device_id.as_str() {
                "NEWDEV" => Ok(serde_json::json!("done")),
                _ => Ok(serde_json::json!("invalid_password")),
            },
            Call::ChangePassword { sign_out_others } => match sign_out_others {
                true => Ok(serde_json::json!("done")),
                // A code outside the contract must not reach the page.
                false => Ok(serde_json::json!("password_was_hunter2")),
            },
            Call::ThreePids {} => Ok(serde_json::json!({
                "threePids": [{"medium": "email", "address": "<b>a</b>@example.invalid"}],
                "truncated": false, "canChange": true, "pending": null
            })),
            Call::RequestEmailToken { email } => match email.as_str() {
                "a@example.invalid" => Ok(serde_json::json!("token_sent")),
                _ => Ok(serde_json::json!("medium_not_supported")),
            },
            Call::RequestPhoneToken { .. } => Ok(serde_json::json!("token_sent")),
            Call::AddThreePid {} => Ok(serde_json::json!("not_confirmed")),
            Call::RemoveThreePid { .. } => Ok(serde_json::json!("done")),
            Call::VerifyDevice { device_id } => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled", "operation": "working",
                "error": null, "canSetUpRecovery": false,
                "verification": {"flowId": device_id, "phase": "waiting", "emoji": [],
                    "otherDevice": null, "contact": null}
            })),
            Call::ChangeRecoveryKey {} => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled",
                "operation": "waiting_for_confirmation", "error": null,
                "verification": null, "canSetUpRecovery": false
            })),
            Call::NotificationLevel {} => Ok(serde_json::json!({"level": "custom"})),
            Call::SetNotificationLevel { .. } => Ok(serde_json::Value::Null),
            Call::DeviceList {} => Ok(serde_json::json!({
                "devices": [{
                    "deviceId": "ME",
                    "displayName": "<i>Laptop</i>",
                    "lastSeen": 1_767_258_000_000u64,
                    "current": true,
                    "verified": true
                }],
                "truncated": false
            })),
            Call::ProfileList {} => Ok(serde_json::json!({
                "profiles": [{"slot": 1, "active": true}, {"slot": 2, "active": false}],
                "canAdd": true
            })),
            Call::RoomSettings { room_id } => {
                let mut settings = serde_json::json!({
                    "roomId": room_id, "name": "<b>Plans</b>", "topic": "a\nb",
                    "canonicalAlias": "#plans:example.invalid", "altAliases": [],
                    "localAliases": null, "joinRule": "invite", "historyVisibility": "shared",
                    "guestAccess": false, "encryption": "encrypted", "published": null,
                    "roomVersion": "11", "direct": false, "notifications": "default",
                    "levels": [{"key": "ban", "level": 50}], "ownLevel": null,
                    "may": {"name": true, "topic": true, "canonicalAlias": true,
                        "joinRules": true, "historyVisibility": true, "guestAccess": true,
                        "encryption": false, "powerLevels": true, "invite": true,
                        "kick": true, "ban": true}
                });
                match room_id.as_str() {
                    "!r:example.invalid" => Ok(settings),
                    // A level JavaScript cannot hold exactly is refused.
                    "!big:example.invalid" => {
                        settings["levels"][0]["level"] = 9_007_199_254_740_992_u64.into();
                        Ok(settings)
                    }
                    // A field outside the contract is refused.
                    "!extra:example.invalid" => {
                        settings["serverAcl"] = serde_json::json!({"allow": ["*"]});
                        Ok(settings)
                    }
                    _ => Err(CoreError::Core(ErrorCode::NotFound)),
                }
            }
            Call::RoomMembers { room_id, filter } => match filter.as_str() {
                "" => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false,
                    "members": [{"userId": "@bob:example.invalid", "displayName": "<i>Bob</i>",
                        "membership": "join", "level": 50, "own": false, "canKick": true,
                        "canBan": true, "canUnban": false, "canChangeLevel": true}]
                })),
                // A membership outside the contract is refused.
                _ => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false,
                    "members": [{"userId": "@bob:example.invalid", "displayName": null,
                        "membership": "leave", "level": 0, "own": false, "canKick": false,
                        "canBan": false, "canUnban": false, "canChangeLevel": false}]
                })),
            },
            Call::ChangeRoom { room_id, change } => match (room_id.as_str(), change) {
                ("!r:example.invalid", _) => Ok(serde_json::Value::Null),
                (_, persephone_contracts::RoomChange::Leave {}) => {
                    Err(CoreError::Core(ErrorCode::NotFound))
                }
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::AddProfile {} => Ok(serde_json::Value::Null),
            Call::SwitchProfile { slot } => match slot {
                2 => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
        }
    }

    /// Band 0 of [`HANDLE`] is 4 rows of 3 pixels, band 1 the last row.
    fn image_band(&self, handle: String, band: u32) -> Result<Band, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.available {
            return Err(CoreError::Unavailable);
        }
        if handle != HANDLE {
            return Err(CoreError::Core(ErrorCode::NotFound));
        }
        let rows = match band {
            0 => 4,
            1 => 1,
            _ => return Err(CoreError::Core(ErrorCode::InvalidArgument)),
        };
        let pixels = (0..3 * rows * 4).map(|byte| byte as u8).collect();
        Ok(Band {
            width: 3,
            rows,
            pixels,
        })
    }
}

/// A well-formed image handle the fake core knows.
const HANDLE: &str = "0123456789abcdef0123456789abcdef";

fn app_with(core: CoreLink) -> tauri::App<MockRuntime> {
    configure(mock_builder(), core, Arc::default())
        .build(context())
        .expect("shell builds on the mock runtime")
}

fn app() -> tauri::App<MockRuntime> {
    app_with(FakeCore::new(true))
}

fn app_origin() -> &'static str {
    if cfg!(windows) {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    }
}

fn request(cmd: &str, origin: &str, body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: origin.parse().unwrap(),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

fn call(
    cmd: &str,
    origin: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    let app = app();
    let window = create_main_window(&app).expect("main window builds");
    assert_eq!(window.label(), MAIN_WINDOW);
    get_ipc_response(&window, request(cmd, origin, body))
        .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
}

#[test]
fn given_allowed_command_from_app_origin_then_typed_status_is_returned() {
    let reply = call("app_status", app_origin(), serde_json::json!({})).unwrap();
    // The reply is the core's answer, relayed and shape-checked by the UI.
    assert_eq!(
        reply,
        serde_json::json!({"version": "9.9.9", "revision": "0123456789ab", "milestone": "P1", "store": "key_missing", "account": {"state": "signed_out", "userId": null, "error": null}})
    );
}

#[test]
fn given_unregistered_command_then_it_is_rejected() {
    for cmd in ["app_statuses", "get_secrets", "", "APP_STATUS"] {
        assert!(
            call(cmd, app_origin(), serde_json::json!({})).is_err(),
            "{cmd:?} was accepted"
        );
    }
}

/// None of Tauri's built-in core plugins is granted, so a compromised page
/// cannot open windows, listen to events, resolve paths or change the webview.
#[test]
fn given_core_plugin_commands_then_every_one_is_denied() {
    for cmd in [
        "plugin:app|version",
        "plugin:app|app_show",
        "plugin:event|listen",
        "plugin:event|emit",
        "plugin:path|resolve_directory",
        "plugin:window|close",
        "plugin:window|create",
        "plugin:window|set_title",
        "plugin:webview|create_webview_window",
        "plugin:webview|print",
        "plugin:image|from_bytes",
        "plugin:resources|close",
        "plugin:menu|new",
        "plugin:tray|new",
        // ADR-0022: the notification plugin is used from Rust only.
        "plugin:notification|notify",
        "plugin:notification|request_permission",
        "plugin:notification|is_permission_granted",
        "plugin:notification|register_action_types",
        "plugin:notification|register_listener",
        "plugin:notification|remove_active",
    ] {
        assert!(
            call(cmd, app_origin(), serde_json::json!({})).is_err(),
            "{cmd} was allowed"
        );
    }
}

/// The capability is local-only: the same allowed command from a remote
/// origin must be refused.
#[test]
fn given_allowed_command_from_remote_origin_then_it_is_rejected() {
    for origin in [
        "https://example.org",
        "http://tauri.localhost.example.org",
        "http://localhost:1420",
    ] {
        assert!(
            call("app_status", origin, serde_json::json!({})).is_err(),
            "{origin} was accepted"
        );
    }
}

#[test]
fn given_wrong_invoke_key_then_the_request_is_dropped_unanswered() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let app = app();
    let window = create_main_window(&app).unwrap();
    let mut forged = request("app_status", app_origin(), serde_json::json!({}));
    forged.invoke_key = "forged-key".into();
    let answered = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&answered);
    // Tauri checks the key first and returns without calling the responder.
    window.as_ref().clone().on_message(
        forged,
        Box::new(move |_, _, _, _, _| flag.store(true, Ordering::SeqCst)),
    );
    assert!(
        !answered.load(Ordering::SeqCst),
        "forged request was answered"
    );
    // Control: the same command with the real key is answered.
    assert!(
        get_ipc_response(
            &window,
            request("app_status", app_origin(), serde_json::json!({}))
        )
        .is_ok()
    );
}

#[test]
fn given_shell_config_then_security_settings_are_restrictive() {
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    let app = &config["app"];
    assert_eq!(app["withGlobalTauri"], false);
    assert_eq!(app["windows"], serde_json::json!([]));
    assert_eq!(app["security"]["freezePrototype"], true);
    assert_eq!(app["security"]["capabilities"], serde_json::json!(["main"]));
    assert!(
        app["security"]
            .get("dangerousDisableAssetCspModification")
            .is_none()
    );
    assert!(app["security"].get("pattern").is_none());
    assert!(
        config["build"].get("devUrl").is_none(),
        "no development server"
    );
    assert!(config["build"].get("beforeDevCommand").is_none());
    assert_eq!(config["bundle"]["active"], false);

    let csp = app["security"]["csp"].as_object().unwrap();
    assert_eq!(csp["default-src"], "'none'");
    for (directive, sources) in csp {
        let sources = sources.as_str().unwrap();
        for forbidden in [
            "'unsafe-inline'",
            "'unsafe-eval'",
            "*",
            "data:",
            "blob:",
            "https:",
            "http:",
        ] {
            assert!(
                !sources.split_whitespace().any(|source| source == forbidden),
                "{directive} allows {forbidden}"
            );
        }
    }

    let capability: serde_json::Value =
        serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
    assert_eq!(capability["windows"], serde_json::json!([MAIN_WINDOW]));
    assert_eq!(
        capability["permissions"],
        serde_json::json!([
            "allow-app-status",
            "allow-room-list",
            "allow-timeline-page",
            "allow-subscribe-room",
            "allow-unsubscribe-room",
            "allow-watch-session",
            "allow-lock-session",
            "allow-unlock-session",
            "allow-reset-profile",
            "allow-sign-in",
            "allow-discover-homeserver",
            "allow-proxy-setting",
            "allow-set-proxy-setting",
            "allow-register",
            "allow-sign-out",
            "allow-security-status",
            "allow-set-up-encryption",
            "allow-recover-with-key",
            "allow-send-message",
            "allow-edit-message",
            "allow-delete-message",
            "allow-react",
            "allow-mark-read",
            "allow-room-activity",
            "allow-set-typing",
            "allow-open-link",
            "allow-room-servers",
            "allow-room-readers",
            "allow-new-sessions",
            "allow-acknowledge-sessions",
            "allow-remove-device",
            "allow-pending-messages",
            "allow-retry-message",
            "allow-discard-message",
            "allow-allow-unencrypted",
            "allow-set-up-recovery",
            "allow-verify-this-device",
            "allow-accept-verification",
            "allow-confirm-verification",
            "allow-cancel-verification",
            "allow-contact-status",
            "allow-verify-contact",
            "allow-identity-changes",
            "allow-accept-identity",
            "allow-load-image",
            "allow-image-band",
            "allow-release-image",
            "allow-pick-image",
            "allow-send-image",
            "allow-discard-image",
            "allow-invite-list",
            "allow-accept-invite",
            "allow-decline-invite",
            "allow-start-conversation",
            "allow-search-users",
            "allow-create-room",
            "allow-invite-user",
            "allow-room-settings",
            "allow-room-members",
            "allow-change-room",
            "allow-display-name",
            "allow-set-display-name",
            "allow-device-list",
            "allow-account-settings",
            "allow-set-account-settings",
            "allow-change-password",
            "allow-three-pids",
            "allow-request-email-token",
            "allow-request-phone-token",
            "allow-add-three-pid",
            "allow-remove-three-pid",
            "allow-verify-device",
            "allow-change-recovery-key",
            "allow-notification-level",
            "allow-set-notification-level",
            "allow-profile-list",
            "allow-add-profile",
            "allow-switch-profile"
        ])
    );
    assert!(capability.get("remote").is_none(), "no remote origins");
}

#[test]
fn given_unreachable_core_then_the_page_gets_only_a_generic_error() {
    let app = app_with(FakeCore::new(false));
    let window = create_main_window(&app).unwrap();
    let reply = get_ipc_response(
        &window,
        request("app_status", app_origin(), serde_json::json!({})),
    );
    assert_eq!(reply.unwrap_err(), serde_json::json!("core_unavailable"));
}

#[test]
fn given_view_commands_then_replies_are_relayed_and_checked_against_contracts() {
    let rooms = call("room_list", app_origin(), serde_json::json!({})).unwrap();
    assert_eq!(rooms["rooms"][0]["displayName"], "<b>x</b>");
    let page = call(
        "timeline_page",
        app_origin(),
        serde_json::json!({"roomId": "!r:example.invalid", "cursor": null}),
    )
    .unwrap();
    assert_eq!(page["items"][0]["body"], "hi");

    let error = |args| call("timeline_page", app_origin(), args).unwrap_err();
    assert_eq!(
        error(serde_json::json!({"roomId": "!missing:example.invalid", "cursor": null})),
        "not_found"
    );
    assert_eq!(
        error(serde_json::json!({"roomId": "!r:example.invalid", "cursor": "t1.0"})),
        "invalid_argument"
    );
    // The UI refuses a reply carrying a field outside the contract.
    assert_eq!(
        error(serde_json::json!({"roomId": "!bad-reply:example.invalid", "cursor": null})),
        "core_unavailable"
    );
}

#[test]
fn given_oversized_arguments_then_the_ui_refuses_without_calling_the_core() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    for args in [
        serde_json::json!({"roomId": format!("!{}", "r".repeat(300)), "cursor": null}),
        serde_json::json!({"roomId": "!r:example.invalid", "cursor": "c".repeat(65)}),
    ] {
        let reply = get_ipc_response(&window, request("timeline_page", app_origin(), args));
        assert_eq!(reply.unwrap_err(), serde_json::json!("invalid_argument"));
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), 0);
}

/// Subscribing stores the page's channel only once the core accepts the room;
/// core events reach exactly that channel, as an empty message; unsubscribing
/// stops them.
#[test]
fn given_subscriptions_then_core_events_reach_only_the_subscribed_rooms_channel() {
    use std::sync::Mutex;
    let delivered: Arc<Mutex<Vec<(u32, String)>>> = Arc::default();
    let record = delivered.clone();
    let subscriptions = Arc::new(Subscriptions::default());
    let core = FakeCore::new(true);
    let app = configure(
        mock_builder().channel_interceptor(move |_, callback, _, body| {
            let body = match body {
                tauri::ipc::InvokeResponseBody::Json(json) => json.clone(),
                tauri::ipc::InvokeResponseBody::Raw(_) => "raw".into(),
            };
            record.lock().unwrap().push((callback.0, body));
            true
        }),
        core.clone(),
        subscriptions.clone(),
    )
    .build(context())
    .unwrap();
    let window = create_main_window(&app).unwrap();
    let subscribe = |room: &str, channel: u32| {
        get_ipc_response(
            &window,
            request(
                "subscribe_room",
                app_origin(),
                serde_json::json!({"roomId": room, "onChange": format!("__CHANNEL__:{channel}")}),
            ),
        )
        .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        subscribe("!r:example.invalid", 7),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        subscribe("!s:example.invalid", 8),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        subscribe("!missing:example.invalid", 9).unwrap_err(),
        "not_found"
    );
    assert_eq!(
        subscribe("!full:example.invalid", 10).unwrap_err(),
        "limit_exceeded"
    );
    assert_eq!(
        subscribe(&format!("!{}", "r".repeat(300)), 11).unwrap_err(),
        "invalid_argument"
    );
    // A channel value that is not a page channel is refused by Tauri.
    assert!(
        get_ipc_response(
            &window,
            request(
                "subscribe_room",
                app_origin(),
                serde_json::json!({"roomId": "!r:example.invalid", "onChange": "javascript:alert(1)"}),
            ),
        )
        .is_err()
    );
    assert_eq!(
        subscriptions.rooms(),
        ["!r:example.invalid", "!s:example.invalid"]
    );

    let changed = |room: &str| Event::RoomChanged {
        room_id: room.into(),
    };
    subscriptions.dispatch(changed("!r:example.invalid"));
    subscriptions.dispatch(changed("!missing:example.invalid"));
    subscriptions.dispatch(changed("!s:example.invalid"));
    subscriptions.dispatch(changed("!r:example.invalid"));
    assert_eq!(
        *delivered.lock().unwrap(),
        [(7, "null".into()), (8, "null".into()), (7, "null".into())]
    );

    let reply = get_ipc_response(
        &window,
        request(
            "unsubscribe_room",
            app_origin(),
            serde_json::json!({"roomId": "!r:example.invalid"}),
        ),
    );
    assert!(reply.is_ok());
    assert_eq!(subscriptions.rooms(), ["!s:example.invalid"]);
    delivered.lock().unwrap().clear();
    subscriptions.dispatch(changed("!r:example.invalid"));
    assert!(delivered.lock().unwrap().is_empty());
}

/// A session change drops every room channel and reaches the page's session
/// channel as `"session"`, a room-list change as `"rooms"`; lock and unlock relay the core's state and
/// refuse one outside the contract.
#[test]
fn given_a_session_change_then_room_channels_drop_and_the_session_channel_is_told() {
    use std::sync::Mutex;
    let delivered: Arc<Mutex<Vec<(u32, String)>>> = Arc::default();
    let record = delivered.clone();
    let subscriptions = Arc::new(Subscriptions::default());
    let app = configure(
        mock_builder().channel_interceptor(move |_, callback, _, body| {
            let body = match body {
                tauri::ipc::InvokeResponseBody::Json(json) => json.clone(),
                tauri::ipc::InvokeResponseBody::Raw(_) => "raw".into(),
            };
            record.lock().unwrap().push((callback.0, body));
            true
        }),
        FakeCore::new(true),
        subscriptions.clone(),
    )
    .build(context())
    .unwrap();
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert!(
        invoke(
            "watch_session",
            serde_json::json!({"onChange": "__CHANNEL__:3"})
        )
        .is_ok()
    );
    assert!(
        invoke(
            "subscribe_room",
            serde_json::json!({"roomId": "!r:example.invalid", "onChange": "__CHANNEL__:7"})
        )
        .is_ok()
    );

    subscriptions.dispatch(Event::SessionChanged {});
    assert!(subscriptions.rooms().is_empty(), "room channels dropped");
    subscriptions.dispatch(Event::RoomChanged {
        room_id: "!r:example.invalid".into(),
    });
    subscriptions.dispatch(Event::RoomListChanged {});
    subscriptions.dispatch(Event::SecurityChanged {});
    let delivered = delivered.lock().unwrap().clone();
    assert!(
        delivered.contains(&(3, r#""security""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""session""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""rooms""#.into())),
        "{delivered:?}"
    );
    assert!(
        !delivered
            .iter()
            .any(|(id, body)| *id == 7 && body == "null"),
        "no room message after the session changed: {delivered:?}"
    );

    assert_eq!(
        invoke("lock_session", serde_json::json!({})),
        Ok(serde_json::json!("locked"))
    );
    assert_eq!(
        invoke("unlock_session", serde_json::json!({})).unwrap_err(),
        "core_unavailable"
    );
    assert_eq!(
        invoke("reset_profile", serde_json::json!({})),
        Ok(serde_json::json!("unlocking"))
    );
}

/// Sign-in relays only the homeserver URL, refuses oversized input before the
/// core, and checks the reply against the contract.
#[test]
fn given_sign_in_then_only_the_homeserver_crosses_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |args: serde_json::Value| {
        get_ipc_response(&window, request("sign_in", app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke(serde_json::json!({"homeserver": "https://matrix.example.org"})),
        Ok(serde_json::json!({"state": "signing_in", "userId": null, "error": null}))
    );
    assert_eq!(
        invoke(serde_json::json!({"homeserver": "https://evil.example"})).unwrap_err(),
        "core_unavailable"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(serde_json::json!({"homeserver": "h".repeat(3000)})).unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized URL never reached the core"
    );
    let signed_out = get_ipc_response(
        &window,
        request("sign_out", app_origin(), serde_json::json!({})),
    )
    .map(|reply| reply.deserialize::<serde_json::Value>().unwrap());
    assert_eq!(signed_out, Ok(serde_json::json!("restart_required")));
}

/// Registration relays only the homeserver URL and the username; the reason
/// a registration failed crosses as a fixed code, and anything else is
/// refused. No password or token argument exists.
#[test]
fn given_register_then_only_the_homeserver_and_username_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |args: serde_json::Value| {
        get_ipc_response(&window, request("register", app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let homeserver = "https://matrix.example.org";
    assert_eq!(
        invoke(serde_json::json!({"homeserver": homeserver, "username": "newbie"})).unwrap()["state"],
        "signing_in"
    );
    assert_eq!(
        invoke(serde_json::json!({"homeserver": homeserver, "username": "taken"})).unwrap()["error"],
        "username_taken"
    );
    assert_eq!(
        invoke(serde_json::json!({"homeserver": homeserver, "username": "odd"})).unwrap_err(),
        "core_unavailable"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"homeserver": "h".repeat(3000), "username": "newbie"}),
        serde_json::json!({"homeserver": homeserver, "username": "n".repeat(256)}),
    ] {
        assert_eq!(invoke(args).unwrap_err(), "invalid_argument");
    }
    assert!(invoke(serde_json::json!({"homeserver": homeserver})).is_err());
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "refused arguments never reached the core"
    );
}

/// The encryption commands take no arguments and relay codes only: a reply
/// carrying a recovery key is refused by the contract.
#[test]
fn given_encryption_commands_then_only_codes_cross_and_secrets_are_refused() {
    let app = app_with(FakeCore::new(true));
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str| {
        get_ipc_response(&window, request(cmd, app_origin(), serde_json::json!({})))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke("security_status"),
        Ok(
            serde_json::json!({"identity": "none", "recovery": "disabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false})
        )
    );
    assert_eq!(
        invoke("set_up_encryption").unwrap()["operation"],
        "waiting_for_password"
    );
    assert_eq!(invoke("recover_with_key").unwrap_err(), "core_unavailable");
    assert_eq!(invoke("set_up_recovery").unwrap()["operation"], "working");
}

/// Sending relays the room and body only; oversized input never reaches the
/// core, and a reply carrying more than the contract (a transaction ID) is
/// refused.
#[test]
fn given_sending_commands_then_bounded_arguments_cross_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let room = "!r:example.invalid";
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "hello"})
        )
        .unwrap()["state"],
        "queued"
    );
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "other"})
        )
        .unwrap_err(),
        "core_unavailable"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "x".repeat(16 * 1024 + 1)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "hi", "replyTo": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized body and reply target never crossed"
    );
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "hello", "replyTo": "$e"})
        )
        .unwrap()["state"],
        "queued"
    );
    assert_eq!(
        invoke(
            "edit_message",
            serde_json::json!({"roomId": room, "eventId": "$mine", "body": "hello"})
        )
        .unwrap()["state"],
        "queued"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255)), "body": "x"}),
        serde_json::json!({"roomId": room, "eventId": "$mine", "body": "x".repeat(16 * 1024 + 1)}),
    ] {
        assert_eq!(
            invoke("edit_message", args).unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized edits never crossed"
    );
    assert_eq!(
        invoke(
            "delete_message",
            serde_json::json!({"roomId": room, "eventId": "$mine"})
        ),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke(
            "delete_message",
            serde_json::json!({"roomId": room, "eventId": "$theirs"})
        )
        .unwrap_err(),
        "rejected"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "delete_message",
            serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "an oversized event ID never crossed"
    );
    assert_eq!(
        invoke(
            "react",
            serde_json::json!({"roomId": room, "eventId": "$e", "key": "👍"})
        ),
        Ok(serde_json::Value::Null)
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"roomId": room, "eventId": "$e", "key": "x".repeat(65)}),
        serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255)), "key": "👍"}),
    ] {
        assert_eq!(invoke("react", args).unwrap_err(), "invalid_argument");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized reactions never crossed"
    );
    assert_eq!(
        invoke(
            "mark_read",
            serde_json::json!({"roomId": room, "eventId": "$e"})
        ),
        Ok(serde_json::Value::Null)
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "mark_read",
            serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke(
            "room_activity",
            serde_json::json!({"roomId": room, "eventId": "$e"})
        )
        .unwrap()["typing"][0],
        "@b:example.invalid"
    );
    assert_eq!(
        invoke(
            "set_typing",
            serde_json::json!({"roomId": room, "typing": true})
        ),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke(
            "set_typing",
            serde_json::json!({"roomId": room, "typing": false})
        )
        .unwrap_err(),
        "rejected"
    );
    assert_eq!(
        invoke(
            "open_link",
            serde_json::json!({"url": "https://example.org/"})
        ),
        Ok(serde_json::Value::Bool(true))
    );
    assert_eq!(
        invoke("room_servers", serde_json::json!({"roomId": room})).unwrap()["servers"][0]["members"],
        2
    );
    assert_eq!(
        invoke("room_readers", serde_json::json!({"roomId": room})).unwrap()["readers"][0]["refused"],
        1
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "open_link",
            serde_json::json!({"url": format!("https://example.org/{}", "a".repeat(2048))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke(
            "room_activity",
            serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke("pending_messages", serde_json::json!({"roomId": room})),
        Ok(serde_json::json!([]))
    );
    assert_eq!(
        invoke("retry_message", serde_json::json!({"localId": 1})),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke("discard_message", serde_json::json!({"localId": 7})).unwrap_err(),
        "not_found"
    );
    assert_eq!(
        invoke("allow_unencrypted", serde_json::json!({"roomId": room})),
        Ok(serde_json::Value::Null)
    );
}

/// Verification relays the flow ID only, refuses an oversized one before the
/// core, and passes the emoji and device name through unchanged for the page
/// to render as text.
#[test]
fn given_verification_commands_then_only_the_flow_id_crosses() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke("verify_this_device", serde_json::json!({})).unwrap()["verification"]["phase"],
        "waiting"
    );
    for cmd in [
        "accept_verification",
        "confirm_verification",
        "cancel_verification",
    ] {
        let reply = invoke(cmd, serde_json::json!({"flowId": "f1"})).unwrap();
        assert_eq!(reply["verification"]["flowId"], "f1", "{cmd}");
        assert_eq!(reply["verification"]["emoji"][0]["symbol"], "🐶");
    }
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "confirm_verification",
            serde_json::json!({"flowId": "f".repeat(129)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized flow ID never crossed"
    );
}

/// Contact commands relay the room ID only and refuse an oversized one before
/// the core; the contact's user ID comes back unchanged for the page to render
/// as text, and a trust level outside the contract is refused.
#[test]
fn given_contact_commands_then_only_the_room_id_crosses() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let status = invoke(
        "contact_status",
        serde_json::json!({"roomId": "!dm:example.invalid"}),
    )
    .unwrap();
    assert_eq!(status["userId"], "<b>@bob</b>:example.invalid");
    assert_eq!(status["trust"], "unverified");
    assert!(
        invoke(
            "contact_status",
            serde_json::json!({"roomId": "!odd:example.invalid"})
        )
        .is_err()
    );
    assert_eq!(
        invoke(
            "verify_contact",
            serde_json::json!({"roomId": "!dm:example.invalid"})
        )
        .unwrap()["operation"],
        "working"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for cmd in ["contact_status", "verify_contact"] {
        assert_eq!(
            invoke(cmd, serde_json::json!({"roomId": "!".repeat(256)})).unwrap_err(),
            "invalid_argument",
            "{cmd}"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized room IDs never crossed"
    );
}

/// Conversation commands relay only their one argument, refuse an oversized
/// one before the core, and refuse a reply outside the contract: an
/// unencrypted outcome or a directory match with an avatar URL never reaches
/// the page.
#[test]
fn given_conversation_commands_then_one_argument_crosses_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let invites = invoke("invite_list", serde_json::json!({})).unwrap();
    assert_eq!(invites["invites"][0]["inviterName"], "<b>Bob</b>");
    for cmd in ["accept_invite", "decline_invite"] {
        assert!(
            invoke(cmd, serde_json::json!({"roomId": "!inv:example.invalid"})).is_ok(),
            "{cmd}"
        );
        assert_eq!(
            invoke(cmd, serde_json::json!({"roomId": "!gone:example.invalid"})).unwrap_err(),
            "not_found",
            "{cmd}"
        );
    }
    assert_eq!(
        invoke(
            "start_conversation",
            serde_json::json!({"userId": "@bob:example.invalid"})
        )
        .unwrap(),
        serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"})
    );
    assert_eq!(
        invoke(
            "start_conversation",
            serde_json::json!({"userId": "@nokeys:example.invalid"})
        )
        .unwrap()["kind"],
        "no_identity"
    );
    assert!(
        invoke(
            "start_conversation",
            serde_json::json!({"userId": "@odd:example.invalid"})
        )
        .is_err()
    );
    assert_eq!(
        invoke("search_users", serde_json::json!({"term": "bob"})).unwrap()["users"][0]["userId"],
        "@bob:example.invalid"
    );
    assert!(invoke("search_users", serde_json::json!({"term": "a"})).is_err());
    let calls = core.calls.load(Ordering::SeqCst);
    for (cmd, args) in [
        (
            "accept_invite",
            serde_json::json!({"roomId": "!".repeat(256)}),
        ),
        (
            "decline_invite",
            serde_json::json!({"roomId": "!".repeat(256)}),
        ),
        (
            "start_conversation",
            serde_json::json!({"userId": "@".repeat(256)}),
        ),
        ("search_users", serde_json::json!({"term": "b".repeat(257)})),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized arguments never crossed"
    );
}

/// Room, account and profile commands relay only their named arguments,
/// refuse oversized ones (and slots out of range) before the core, and
/// refuse replies outside the contract: an unencrypted room never reaches
/// the page, and a device carries no IP address.
#[test]
fn given_room_account_and_profile_commands_then_bounded_arguments_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let invites = invoke("invite_list", serde_json::json!({})).unwrap();
    assert_eq!(invites["invites"][1]["direct"], false);
    assert_eq!(invites["invites"][1]["inviter"], serde_json::Value::Null);
    assert_eq!(
        invoke(
            "create_room",
            serde_json::json!({"name": "Plans", "invite": ["@bob:example.invalid"]})
        )
        .unwrap(),
        serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"})
    );
    assert_eq!(
        invoke(
            "create_room",
            serde_json::json!({"name": "Plans", "invite": ["@nokeys:example.invalid"]})
        )
        .unwrap(),
        serde_json::json!({"kind": "no_identity", "userId": "@nokeys:example.invalid"})
    );
    assert!(
        invoke(
            "create_room",
            serde_json::json!({"name": "Other", "invite": []})
        )
        .is_err(),
        "an outcome outside the contract"
    );
    assert_eq!(
        invoke(
            "invite_user",
            serde_json::json!({"roomId": "!r:example.invalid", "userId": "@bob:example.invalid"})
        )
        .unwrap()["kind"],
        "invited"
    );
    assert_eq!(
        invoke("display_name", serde_json::json!({})).unwrap()["displayName"],
        "<b>Al</b>"
    );
    assert_eq!(
        invoke("set_display_name", serde_json::json!({"name": ""})).unwrap()["displayName"],
        serde_json::Value::Null
    );
    assert_eq!(
        invoke("set_display_name", serde_json::json!({"name": "Bo"})).unwrap_err(),
        "rejected"
    );
    let devices = invoke("device_list", serde_json::json!({})).unwrap();
    assert_eq!(devices["devices"][0]["current"], true);
    assert!(devices["devices"][0].get("lastSeenIp").is_none());
    assert_eq!(
        invoke("account_settings", serde_json::json!({})).unwrap()["chatTextPercent"],
        120
    );
    let settings = |enter_sends: bool| {
        serde_json::json!({"settings": {"enterSends": enter_sends, "chatTextPercent": 100,
            "interfaceTextPercent": 100, "desktopNotifications": false,
            "sendTyping": false, "sendReadReceipts": false, "idleLockMinutes": 0,
            "notificationNames": true}})
    };
    assert!(invoke("set_account_settings", settings(true)).is_ok());
    assert_eq!(
        invoke("set_account_settings", settings(false)).unwrap_err(),
        "locked"
    );
    // Every setting is required: an older page's call does not cross.
    assert!(
        invoke(
            "set_account_settings",
            serde_json::json!({"enterSends": true})
        )
        .is_err()
    );
    assert_eq!(
        invoke("profile_list", serde_json::json!({})).unwrap()["profiles"][1]["slot"],
        2
    );
    assert!(invoke("add_profile", serde_json::json!({})).is_ok());
    assert!(invoke("switch_profile", serde_json::json!({"slot": 2})).is_ok());
    assert_eq!(
        invoke("switch_profile", serde_json::json!({"slot": 3})).unwrap_err(),
        "not_found"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    let many: Vec<String> = (0..11).map(|n| format!("@u{n}:s")).collect();
    for (cmd, args) in [
        (
            "create_room",
            serde_json::json!({"name": "x".repeat(256), "invite": []}),
        ),
        (
            "create_room",
            serde_json::json!({"name": "x", "invite": many}),
        ),
        (
            "create_room",
            serde_json::json!({"name": "x", "invite": ["@".repeat(256)]}),
        ),
        (
            "invite_user",
            serde_json::json!({"roomId": "!".repeat(256), "userId": "@b:s"}),
        ),
        (
            "invite_user",
            serde_json::json!({"roomId": "!r:s", "userId": "@".repeat(256)}),
        ),
        (
            "set_display_name",
            serde_json::json!({"name": "a".repeat(257)}),
        ),
        ("switch_profile", serde_json::json!({"slot": 0})),
        ("switch_profile", serde_json::json!({"slot": 9})),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized arguments never crossed"
    );
}

/// The account-change commands (ADR-0019 to ADR-0022) relay only codes,
/// addresses and IDs; no password, code, key or path argument exists. Bad
/// arguments never reach the core, and replies outside the contract are
/// refused.
#[test]
fn given_account_change_commands_then_only_non_secret_arguments_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke(
            "change_password",
            serde_json::json!({"signOutOthers": true})
        )
        .unwrap(),
        "done"
    );
    assert!(
        invoke(
            "change_password",
            serde_json::json!({"signOutOthers": false})
        )
        .is_err(),
        "a code outside the contract"
    );
    assert_eq!(
        invoke("new_sessions", serde_json::json!({})).unwrap()["sessions"][0]["deviceId"],
        "NEWDEV"
    );
    assert_eq!(
        invoke(
            "discover_homeserver",
            serde_json::json!({"serverName": "example.invalid"})
        )
        .unwrap()["homeserver"],
        "https://matrix.example.invalid/"
    );
    assert_eq!(
        invoke(
            "discover_homeserver",
            serde_json::json!({"serverName": "x".repeat(262)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke("proxy_setting", serde_json::json!({})).unwrap()["proxy"],
        "127.0.0.1:9050"
    );
    for proxy in [serde_json::json!("127.0.0.1:9050"), serde_json::Value::Null] {
        assert_eq!(
            invoke("set_proxy_setting", serde_json::json!({ "proxy": proxy })),
            Ok(serde_json::Value::Null)
        );
    }
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "set_proxy_setting",
            serde_json::json!({"proxy": "h".repeat(301)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke(
            "acknowledge_sessions",
            serde_json::json!({"deviceIds": ["NEWDEV"]})
        ),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke("remove_device", serde_json::json!({"deviceId": "NEWDEV"})).unwrap(),
        "done"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    let eleven: Vec<String> = (0..11).map(|n| format!("D{n}")).collect();
    assert_eq!(
        invoke(
            "acknowledge_sessions",
            serde_json::json!({"deviceIds": eleven})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke(
            "remove_device",
            serde_json::json!({"deviceId": "D".repeat(256)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    let listed = invoke("three_pids", serde_json::json!({})).unwrap();
    assert_eq!(listed["threePids"][0]["medium"], "email");
    assert_eq!(
        invoke(
            "request_email_token",
            serde_json::json!({"email": "a@example.invalid"})
        )
        .unwrap(),
        "token_sent"
    );
    assert_eq!(
        invoke(
            "request_phone_token",
            serde_json::json!({"country": "GB", "phoneNumber": "07700900123"})
        )
        .unwrap(),
        "token_sent"
    );
    assert_eq!(
        invoke("add_three_pid", serde_json::json!({})).unwrap(),
        "not_confirmed"
    );
    assert_eq!(
        invoke(
            "remove_three_pid",
            serde_json::json!({"medium": "msisdn", "address": "447700900123"})
        )
        .unwrap(),
        "done"
    );
    assert_eq!(
        invoke("verify_device", serde_json::json!({"deviceId": "OTHER"})).unwrap()["verification"]
            ["flowId"],
        "OTHER"
    );
    assert_eq!(
        invoke("change_recovery_key", serde_json::json!({})).unwrap()["operation"],
        "waiting_for_confirmation"
    );
    assert_eq!(
        invoke("notification_level", serde_json::json!({})).unwrap()["level"],
        "custom"
    );
    assert!(
        invoke(
            "set_notification_level",
            serde_json::json!({"level": "mentions"})
        )
        .is_ok()
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for (cmd, args) in [
        (
            "request_email_token",
            serde_json::json!({"email": "a".repeat(255)}),
        ),
        (
            "request_phone_token",
            serde_json::json!({"country": "GBR", "phoneNumber": "1"}),
        ),
        (
            "request_phone_token",
            serde_json::json!({"country": "GB", "phoneNumber": "1".repeat(33)}),
        ),
        (
            "remove_three_pid",
            serde_json::json!({"medium": "email", "address": "a".repeat(255)}),
        ),
        ("verify_device", serde_json::json!({"deviceId": ""})),
        (
            "verify_device",
            serde_json::json!({"deviceId": "D".repeat(256)}),
        ),
        (
            "set_notification_level",
            serde_json::json!({"level": "custom"}),
        ),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    // Missing or unknown kinds of argument fail before the handler runs.
    for (cmd, args) in [
        ("change_password", serde_json::json!({})),
        (
            "remove_three_pid",
            serde_json::json!({"medium": "fax", "address": "1"}),
        ),
        (
            "set_notification_level",
            serde_json::json!({"level": "loud"}),
        ),
    ] {
        assert!(invoke(cmd, args).is_err(), "{cmd}");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "refused arguments never crossed"
    );
}

/// GIVEN a desktop-notification event from the core WHEN it is dispatched
/// THEN only the toaster sees its names, the latest one waits for the toaster
/// to be installed, no page channel gets anything, and oversized names are
/// dropped.
#[test]
fn given_a_notification_event_then_only_the_toaster_sees_it() {
    use std::sync::Mutex;
    let delivered: Arc<Mutex<Vec<u32>>> = Arc::default();
    let record = delivered.clone();
    let subscriptions = Arc::new(Subscriptions::default());
    let app = configure(
        mock_builder().channel_interceptor(move |_, callback, _, _| {
            record.lock().unwrap().push(callback.0);
            true
        }),
        FakeCore::new(true),
        subscriptions.clone(),
    )
    .build(context())
    .unwrap();
    let window = create_main_window(&app).unwrap();
    get_ipc_response(
        &window,
        request(
            "watch_session",
            app_origin(),
            serde_json::json!({"onChange": "__CHANNEL__:3"}),
        ),
    )
    .unwrap();
    get_ipc_response(
        &window,
        request(
            "subscribe_room",
            app_origin(),
            serde_json::json!({"roomId": "!r:example.invalid", "onChange": "__CHANNEL__:4"}),
        ),
    )
    .unwrap();
    // Before a toaster is set, only the latest notification is kept, and
    // shown once it is.
    for room in ["Earlier", "Plans"] {
        subscriptions.dispatch(Event::Notification {
            room: room.into(),
            sender: "Bob".into(),
        });
    }
    let toasts: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let shown = toasts.clone();
    subscriptions.set_toaster(Box::new(move |room, sender| {
        shown.lock().unwrap().push((room.into(), sender.into()));
    }));
    subscriptions.dispatch(Event::Notification {
        room: "Later".into(),
        sender: "Ann".into(),
    });
    subscriptions.dispatch(Event::Notification {
        room: "x".repeat(513),
        sender: "Bob".into(),
    });
    assert_eq!(
        *toasts.lock().unwrap(),
        vec![
            ("Plans".to_owned(), "Bob".to_owned()),
            ("Later".to_owned(), "Ann".to_owned())
        ]
    );
    assert!(
        delivered.lock().unwrap().is_empty(),
        "no page channel was told"
    );
}

/// Room settings commands relay a room ID, a filter or one change of a closed
/// set; oversized arguments and unknown change kinds never reach the core,
/// and replies outside the contract are refused.
#[test]
fn given_room_settings_commands_then_bounded_changes_cross_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let settings = invoke(
        "room_settings",
        serde_json::json!({"roomId": "!r:example.invalid"}),
    )
    .unwrap();
    assert_eq!(settings["name"], "<b>Plans</b>");
    assert_eq!(settings["ownLevel"], serde_json::Value::Null);
    for room in ["!big:example.invalid", "!extra:example.invalid"] {
        assert_eq!(
            invoke("room_settings", serde_json::json!({ "roomId": room })).unwrap_err(),
            "core_unavailable",
            "{room}"
        );
    }
    let members = invoke(
        "room_members",
        serde_json::json!({"roomId": "!r:example.invalid", "filter": ""}),
    )
    .unwrap();
    assert_eq!(members["members"][0]["canKick"], true);
    assert_eq!(
        invoke(
            "room_members",
            serde_json::json!({"roomId": "!r:example.invalid", "filter": "left"}),
        )
        .unwrap_err(),
        "core_unavailable"
    );
    for change in [
        serde_json::json!({"kind": "name", "name": "Plans"}),
        serde_json::json!({"kind": "join_rule", "rule": "knock"}),
        serde_json::json!({"kind": "canonical_alias", "alias": null, "altAliases": ["#a:s"]}),
        serde_json::json!({"kind": "level", "key": "kick", "level": 50}),
        serde_json::json!({"kind": "member_level", "userId": "@b:s", "level": -1}),
        serde_json::json!({"kind": "encryption"}),
    ] {
        assert_eq!(
            invoke(
                "change_room",
                serde_json::json!({"roomId": "!r:example.invalid", "change": change.clone()}),
            )
            .unwrap(),
            serde_json::Value::Null,
            "{change}"
        );
    }
    assert_eq!(
        invoke(
            "change_room",
            serde_json::json!({"roomId": "!s:example.invalid", "change": {"kind": "ban", "userId": "@b:s"}}),
        )
        .unwrap_err(),
        "rejected"
    );
    assert_eq!(
        invoke(
            "change_room",
            serde_json::json!({"roomId": "!s:example.invalid", "change": {"kind": "leave"}}),
        )
        .unwrap_err(),
        "not_found"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    let room = "!r:example.invalid";
    for (cmd, args) in [
        (
            "room_settings",
            serde_json::json!({"roomId": "!".repeat(256)}),
        ),
        (
            "room_members",
            serde_json::json!({"roomId": room, "filter": "b".repeat(257)}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "name", "name": "x".repeat(256)}}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "topic", "topic": "x".repeat(4097)}}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "kick", "userId": "@".repeat(256)}}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "canonical_alias", "alias": null,
                "altAliases": vec!["#a:s"; 51]}}),
        ),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    // Kinds and fields outside the closed set fail to deserialize before
    // the handler runs.
    for change in [
        serde_json::json!({"kind": "state", "type": "m.room.server_acl", "content": {}}),
        serde_json::json!({"kind": "leave", "forget": true}),
        serde_json::json!({"kind": "level", "key": "ban", "level": 9_007_199_254_740_992_u64}),
        serde_json::json!({"kind": "canonical_alias", "altAliases": []}),
    ] {
        assert!(
            invoke(
                "change_room",
                serde_json::json!({"roomId": room, "change": change.clone()}),
            )
            .is_err(),
            "{change}"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "refused arguments never crossed"
    );
}

/// Identity-change commands relay only the room and user IDs, refuse
/// oversized ones before the core, and refuse a reply outside the contract.
/// Accepting only reports that the core is asking in its own window.
#[test]
fn given_identity_change_commands_then_only_the_ids_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let changes = invoke(
        "identity_changes",
        serde_json::json!({"roomId": "!dm:example.invalid"}),
    )
    .unwrap();
    assert_eq!(changes["changes"][0]["userId"], "@bob:example.invalid");
    assert_eq!(changes["changes"][0]["wasVerified"], true);
    assert!(
        invoke(
            "identity_changes",
            serde_json::json!({"roomId": "!odd:example.invalid"})
        )
        .is_err()
    );
    assert_eq!(
        invoke(
            "accept_identity",
            serde_json::json!({"roomId": "!dm:example.invalid", "userId": "@bob:example.invalid"})
        )
        .unwrap()["operation"],
        "waiting_for_confirmation"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "identity_changes",
            serde_json::json!({"roomId": "!".repeat(256)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    for args in [
        serde_json::json!({"roomId": "!".repeat(256), "userId": "@b:s"}),
        serde_json::json!({"roomId": "!r:s", "userId": "@".repeat(256)}),
    ] {
        assert_eq!(
            invoke("accept_identity", args).unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized IDs never crossed"
    );
}

fn invoke_on(
    window: &tauri::WebviewWindow<MockRuntime>,
    cmd: &str,
    origin: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    get_ipc_response(window, request(cmd, origin, args))
        .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
}

/// The arguments of each image command with a bad handle or an oversized
/// ID; none of them may reach the core.
fn bad_image_arguments() -> Vec<(&'static str, serde_json::Value)> {
    let long_id = "!".repeat(256);
    let mut cases = vec![
        (
            "load_image",
            serde_json::json!({"roomId": long_id, "eventId": "$image"}),
        ),
        (
            "load_image",
            serde_json::json!({"roomId": "!r:example.invalid", "eventId": "$".repeat(256)}),
        ),
        ("pick_image", serde_json::json!({"roomId": long_id})),
        (
            "send_image",
            serde_json::json!({"roomId": long_id, "handle": HANDLE}),
        ),
    ];
    for handle in [
        String::new(),
        HANDLE.to_uppercase(),
        HANDLE[..31].to_owned(),
        format!("{HANDLE}0"),
        // The right length, but not hex.
        "0123456789abcdef0123456789abcdeg".to_owned(),
        // The right byte length with a multi-byte character.
        format!("{}\u{e9}", &HANDLE[..30]),
        "../../../../../../../Windows/x.png".to_owned(),
    ] {
        cases.push((
            "image_band",
            serde_json::json!({"handle": handle, "band": 0}),
        ));
        cases.push(("release_image", serde_json::json!({"handle": handle})));
        cases.push(("discard_image", serde_json::json!({"handle": handle})));
        cases.push((
            "send_image",
            serde_json::json!({"roomId": "!r:example.invalid", "handle": handle}),
        ));
    }
    cases
}

/// GIVEN the image commands WHEN called from the app THEN views and drafts
/// are relayed and checked against the contracts, the core's codes reach the
/// page as fixed strings (`rejected` among them), and a reply outside the
/// contract (an extra URL, file path or handle) is refused.
#[test]
fn given_image_commands_then_handles_cross_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args| invoke_on(&window, cmd, app_origin(), args);
    let room = "!r:example.invalid";
    let load = |event: &str| {
        invoke(
            "load_image",
            serde_json::json!({"roomId": room, "eventId": event}),
        )
    };
    assert_eq!(
        load("$image"),
        Ok(
            serde_json::json!({"handle": HANDLE, "width": 3, "height": 5, "bands": 2, "bandRows": 4})
        )
    );
    assert_eq!(load("$bad").unwrap_err(), "rejected");
    assert_eq!(load("$busy").unwrap_err(), "limit_exceeded");
    assert_eq!(load("$odd").unwrap_err(), "core_unavailable");

    let draft = invoke("pick_image", serde_json::json!({"roomId": room})).unwrap();
    assert_eq!(draft["kind"], "draft");
    assert_eq!(draft["preview"]["handle"], HANDLE);
    assert_eq!(draft["mime"], "image/png");
    assert_eq!(
        invoke(
            "pick_image",
            serde_json::json!({"roomId": "!cancel:example.invalid"})
        ),
        Ok(serde_json::json!({"kind": "cancelled"}))
    );
    assert_eq!(
        invoke(
            "pick_image",
            serde_json::json!({"roomId": "!path:example.invalid"})
        )
        .unwrap_err(),
        "core_unavailable",
        "a file path never reaches the page"
    );
    assert_eq!(
        invoke(
            "pick_image",
            serde_json::json!({"roomId": "!other:example.invalid"})
        )
        .unwrap_err(),
        "core_unavailable"
    );

    let queued = invoke(
        "send_image",
        serde_json::json!({"roomId": room, "handle": HANDLE}),
    )
    .unwrap();
    assert_eq!(queued["kind"], "image");
    assert_eq!(queued["body"], "");
    assert_eq!(
        invoke(
            "send_image",
            serde_json::json!({"roomId": "!full:example.invalid", "handle": HANDLE})
        )
        .unwrap_err(),
        "limit_exceeded"
    );
    assert_eq!(
        invoke(
            "send_image",
            serde_json::json!({"roomId": room, "handle": "fedcba9876543210fedcba9876543210"})
        )
        .unwrap_err(),
        "core_unavailable",
        "a reply carrying the handle is refused"
    );

    for cmd in ["release_image", "discard_image"] {
        assert_eq!(
            invoke(cmd, serde_json::json!({"handle": HANDLE})),
            Ok(serde_json::Value::Null),
            "{cmd}"
        );
        assert_eq!(
            invoke(
                cmd,
                serde_json::json!({"handle": "fedcba9876543210fedcba9876543210"})
            )
            .unwrap_err(),
            "not_found",
            "{cmd}"
        );
    }
}

/// GIVEN an image band WHEN the page asks for it THEN it is a raw binary
/// response, never JSON: width and rows as little-endian `u32`, then exactly
/// `width * rows * 4` RGBA bytes. A short last band keeps the layout; a
/// missing image or band is a fixed error.
#[test]
fn given_an_image_band_then_the_page_gets_raw_bytes_with_width_and_rows() {
    let app = app();
    let window = create_main_window(&app).unwrap();
    let band = |handle: &str, band: u32| {
        get_ipc_response(
            &window,
            request(
                "image_band",
                app_origin(),
                serde_json::json!({"handle": handle, "band": band}),
            ),
        )
    };
    assert_eq!(BAND_PREFIX_BYTES, 8);
    for (index, rows) in [(0u32, 4u32), (1, 1)] {
        let Ok(tauri::ipc::InvokeResponseBody::Raw(body)) = band(HANDLE, index) else {
            panic!("band {index} is not a raw response");
        };
        assert_eq!(u32::from_le_bytes(body[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(body[4..8].try_into().unwrap()), rows);
        let pixels = &body[BAND_PREFIX_BYTES..];
        assert_eq!(pixels.len(), (3 * rows * 4) as usize, "band {index}");
        assert!(
            pixels.iter().enumerate().all(|(i, byte)| *byte == i as u8),
            "band {index} bytes are unchanged"
        );
    }
    assert_eq!(
        band("fedcba9876543210fedcba9876543210", 0).unwrap_err(),
        "not_found"
    );
    assert_eq!(band(HANDLE, 9).unwrap_err(), "invalid_argument");
    // A band index outside u32 is refused by the argument type.
    for args in [
        serde_json::json!({"handle": HANDLE, "band": -1}),
        serde_json::json!({"handle": HANDLE, "band": 4_294_967_296_u64}),
        serde_json::json!({"handle": HANDLE, "band": "0"}),
        serde_json::json!({"handle": HANDLE}),
    ] {
        assert!(
            get_ipc_response(&window, request("image_band", app_origin(), args.clone())).is_err(),
            "{args}"
        );
    }
}

/// GIVEN malformed handles or oversized room or event IDs WHEN any image
/// command is called THEN the UI refuses it without calling the core.
#[test]
fn given_bad_image_arguments_then_the_ui_refuses_without_calling_the_core() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    for (cmd, args) in bad_image_arguments() {
        assert_eq!(
            invoke_on(&window, cmd, app_origin(), args.clone()).unwrap_err(),
            "invalid_argument",
            "{cmd} {args}"
        );
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), 0);
}

/// GIVEN the image commands WHEN called from a remote origin THEN each is
/// denied by the capability before the core is reached.
#[test]
fn given_image_commands_from_a_remote_origin_then_they_are_denied() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let room = "!r:example.invalid";
    for (cmd, args) in [
        (
            "load_image",
            serde_json::json!({"roomId": room, "eventId": "$image"}),
        ),
        (
            "image_band",
            serde_json::json!({"handle": HANDLE, "band": 0}),
        ),
        ("release_image", serde_json::json!({"handle": HANDLE})),
        ("pick_image", serde_json::json!({"roomId": room})),
        (
            "send_image",
            serde_json::json!({"roomId": room, "handle": HANDLE}),
        ),
        ("discard_image", serde_json::json!({"handle": HANDLE})),
    ] {
        assert!(
            get_ipc_response(&window, request(cmd, app_origin(), args.clone())).is_ok(),
            "{cmd} is allowed from the app"
        );
        let calls = core.calls.load(Ordering::SeqCst);
        for origin in ["https://example.org", "http://localhost:1420"] {
            assert!(
                get_ipc_response(&window, request(cmd, origin, args.clone())).is_err(),
                "{cmd} from {origin}"
            );
        }
        assert_eq!(core.calls.load(Ordering::SeqCst), calls, "{cmd}");
    }
}

/// GIVEN an unreachable core WHEN a band is asked for THEN the page gets the
/// generic error and no bytes.
#[test]
fn given_unreachable_core_then_an_image_band_is_a_generic_error() {
    let app = app_with(FakeCore::new(false));
    let window = create_main_window(&app).unwrap();
    assert_eq!(
        invoke_on(
            &window,
            "image_band",
            app_origin(),
            serde_json::json!({"handle": HANDLE, "band": 0})
        )
        .unwrap_err(),
        "core_unavailable"
    );
}

/// GIVEN a notification with names and one without WHEN its text is made
/// THEN the first names the room and sender, and the second says only that a
/// message arrived.
#[test]
fn given_notifications_with_and_without_names_then_only_named_ones_show_names() {
    assert_eq!(
        persephone_desktop::toast_text("Plans", "@bob:example.invalid"),
        (
            "Plans".to_owned(),
            "New message from @bob:example.invalid".to_owned()
        )
    );
    assert_eq!(
        persephone_desktop::toast_text("", ""),
        ("Persephone".to_owned(), "New message".to_owned())
    );
}
