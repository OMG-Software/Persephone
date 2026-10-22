//! IPC permission tests against the shell's real configuration and compiled
//! capabilities, using Tauri's mock runtime (no window or web view).

use persephone_core_protocol::{Call, ErrorCode, Event};
use persephone_desktop::{
    MAIN_WINDOW, Subscriptions, configure, context,
    core_link::{CoreError, CoreLink, CoreTransport},
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
                serde_json::json!({"version": "9.9.9", "milestone": "P1", "store": "key_missing", "account": {"state": "signed_out", "userId": null, "error": null}}),
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
            Call::SetUpRecovery {} => Ok(
                serde_json::json!({"identity": "verified", "recovery": "disabled", "operation": "working", "error": null, "verification": null, "canSetUpRecovery": false}),
            ),
            Call::SetUpEncryption {} => Ok(
                serde_json::json!({"identity": "none", "recovery": "disabled", "operation": "waiting_for_password", "error": null, "verification": null, "canSetUpRecovery": false}),
            ),
            // A reply carrying a secret is refused by the contract.
            Call::SendMessage { room_id, body } => match body.as_str() {
                "hello" => Ok(serde_json::json!({
                    "localId": 1, "roomId": room_id, "body": body, "state": "queued",
                    "eventId": null, "failure": null
                })),
                // A reply carrying the transaction ID is refused by the contract.
                _ => Ok(serde_json::json!({
                    "localId": 2, "roomId": room_id, "body": body, "state": "queued",
                    "eventId": null, "failure": null, "transactionId": "t"
                })),
            },
            Call::PendingMessages { .. } => Ok(serde_json::json!([])),
            Call::RetryMessage { local_id } | Call::DiscardMessage { local_id } => match local_id {
                1 => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::AllowUnencrypted { .. } => Ok(serde_json::Value::Null),
            Call::RecoverWithKey {} => Ok(
                serde_json::json!({"identity": "unverified", "recovery": "enabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false, "recoveryKey": "x"}),
            ),
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
                    "items": [{"kind": "message", "event_id": "$1", "sender": "@a:example.invalid", "body": "hi"}],
                    "nextCursor": null
                })),
                // A reply with a field the contract does not have.
                ("!bad-reply:example.invalid", _) => Ok(serde_json::json!({
                    "roomId": room_id, "items": [], "nextCursor": null, "accessToken": "x"
                })),
                (_, Some(_)) => Err(CoreError::Core(ErrorCode::InvalidArgument)),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
        }
    }
}

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
        serde_json::json!({"version": "9.9.9", "milestone": "P1", "store": "key_missing", "account": {"state": "signed_out", "userId": null, "error": null}})
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
            "allow-sign-in",
            "allow-sign-out",
            "allow-security-status",
            "allow-set-up-encryption",
            "allow-recover-with-key",
            "allow-send-message",
            "allow-pending-messages",
            "allow-retry-message",
            "allow-discard-message",
            "allow-allow-unencrypted",
            "allow-set-up-recovery",
            "allow-verify-this-device",
            "allow-accept-verification",
            "allow-confirm-verification",
            "allow-cancel-verification"
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
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized body never crossed"
    );
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
