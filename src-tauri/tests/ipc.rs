//! IPC permission tests against the shell's real configuration and compiled
//! capabilities, using Tauri's mock runtime (no window or web view).

use persephone_desktop::{MAIN_WINDOW, configure, context, create_main_window};
use tauri::{
    ipc::{CallbackFn, InvokeBody},
    test::{INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder},
    webview::InvokeRequest,
};

fn app() -> tauri::App<MockRuntime> {
    configure(mock_builder())
        .build(context())
        .expect("shell builds on the mock runtime")
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
    assert_eq!(
        reply,
        serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "milestone": "P1"})
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
        serde_json::json!(["allow-app-status"])
    );
    assert!(capability.get("remote").is_none(), "no remote origins");
}
