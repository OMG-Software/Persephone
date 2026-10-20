//! IPC permission tests against the shell's real configuration and compiled
//! capabilities, using Tauri's mock runtime (no window or web view).

use persephone_core_protocol::{Call, ErrorCode};
use persephone_desktop::{
    MAIN_WINDOW, configure, context,
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
            Call::AppStatus {} => Ok(serde_json::json!({"version": "9.9.9", "milestone": "P1"})),
            Call::RoomList {} => Ok(serde_json::json!({
                "rooms": [{"roomId": "!r:example.invalid", "displayName": "<b>x</b>", "unreadCount": 2}],
                "truncated": false
            })),
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
    configure(mock_builder(), core)
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
        serde_json::json!({"version": "9.9.9", "milestone": "P1"})
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
        serde_json::json!(["allow-app-status", "allow-room-list", "allow-timeline-page"])
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
