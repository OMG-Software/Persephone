//! Persephone desktop shell.
//!
//! A thin Tauri host. The web view is only the interface: it may call the
//! commands registered here, subject to the capabilities in `capabilities/`,
//! and nothing else. The main window can navigate only within the bundled app
//! and cannot open new windows.

use persephone_contracts::AppStatus;
use tauri::{Manager, Runtime, Url, WebviewUrl, WebviewWindowBuilder, webview::NewWindowResponse};

/// The label the `main` capability is bound to.
pub const MAIN_WINDOW: &str = "main";

/// Reports the shell's version and milestone. Carries no system or user data.
#[tauri::command]
fn app_status() -> AppStatus {
    AppStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        milestone: "P1".into(),
    }
}

/// True only for the bundled app's own origin. Tauri serves embedded assets
/// from `http://tauri.localhost` on Windows and `tauri://localhost` elsewhere.
pub fn is_app_url(url: &Url) -> bool {
    match (url.scheme(), url.host_str()) {
        ("tauri", Some("localhost")) => url.port().is_none(),
        ("http" | "https", Some("tauri.localhost")) => url.port().is_none(),
        _ => false,
    }
}

/// Applies the shell's command handler and window setup to a builder; used by
/// both the real runtime and the mock runtime in tests.
pub fn configure<R: Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        .invoke_handler(tauri::generate_handler![app_status])
        .setup(|app| {
            create_main_window(app.handle())?;
            Ok(())
        })
}

/// Creates the main window: bundled app only, no navigation away from it and
/// no new windows. `setup` calls this; tests call it directly because the mock
/// runtime does not run the event loop that triggers `setup`.
pub fn create_main_window<R: Runtime, M: Manager<R>>(
    manager: &M,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    main_window(manager).build()
}

fn main_window<R: Runtime, M: Manager<R>>(manager: &M) -> WebviewWindowBuilder<'_, R, M> {
    WebviewWindowBuilder::new(manager, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
        .title("Persephone")
        .inner_size(1024.0, 720.0)
        .min_inner_size(640.0, 480.0)
        .on_navigation(is_app_url)
        .on_new_window(|_, _| NewWindowResponse::Deny)
}

/// The compiled configuration, embedded assets and permission set.
pub fn context<R: Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

pub fn run() {
    configure(tauri::Builder::default())
        .run(context())
        .expect("Persephone failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_urls_then_only_the_bundled_app_origin_is_navigable() {
        for allowed in [
            "http://tauri.localhost/index.html",
            "https://tauri.localhost/",
            "tauri://localhost/index.html",
        ] {
            assert!(is_app_url(&allowed.parse().unwrap()), "{allowed}");
        }
        for denied in [
            "https://example.org/",
            "http://tauri.localhost.example.org/",
            "http://tauri.localhost:8080/",
            "http://localhost/",
            "file:///C:/Windows/win.ini",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "tauri://example.org/",
            "ipc://localhost/app_status",
        ] {
            assert!(!is_app_url(&denied.parse().unwrap()), "{denied}");
        }
    }
}
