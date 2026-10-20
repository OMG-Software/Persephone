//! Persephone UI process (ADR-0004).
//!
//! A thin, keyless Tauri host. The web view may call the commands registered
//! here, subject to the capabilities in `capabilities/`, and nothing else. Each
//! command is relayed to the key-holding core as one bounded frame; the UI holds
//! no sessions, keys or stores. The main window can navigate only within the
//! bundled app and cannot open new windows.

pub mod core_link;

use core_link::{CoreError, CoreLink};
use persephone_contracts::AppStatus;
use persephone_core_protocol::Call;
use tauri::{
    Manager, Runtime, State, Url, WebviewUrl, WebviewWindowBuilder, webview::NewWindowResponse,
};

/// The label the `main` capability is bound to.
pub const MAIN_WINDOW: &str = "main";

/// Argument the core passes when it starts the UI.
pub const STARTED_BY_CORE: &str = "--started-by-core";

/// The error the web view sees for any failed call. Details stay in Rust.
const UNAVAILABLE: &str = "core_unavailable";

/// Relays to the core, off the UI thread, and checks the reply's shape.
#[tauri::command]
async fn app_status(core: State<'_, CoreLink>) -> Result<AppStatus, String> {
    let core = core.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let value = core.call(Call::AppStatus {})?;
        serde_json::from_value::<AppStatus>(value).map_err(|_| CoreError::BadReply)
    })
    .await
    .map_err(|_| UNAVAILABLE.to_string())?
    .map_err(|_| UNAVAILABLE.to_string())
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

/// Applies the shell's core link, command handler and window setup to a
/// builder; used by both the real runtime and the mock runtime in tests.
pub fn configure<R: Runtime>(builder: tauri::Builder<R>, core: CoreLink) -> tauri::Builder<R> {
    builder
        .manage(core)
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

/// Runs the UI. Refuses unless the core started it, because without the core
/// channel the UI has nothing to relay to.
pub fn run() {
    if !std::env::args().any(|arg| arg == STARTED_BY_CORE) {
        eprintln!("persephone-ui is started by persephone; run persephone instead");
        std::process::exit(2);
    }
    let core = match core_link::PipeTransport::from_stdio() {
        Ok(link) => std::sync::Arc::new(link) as CoreLink,
        Err(error) => {
            eprintln!("persephone-ui: no core channel: {error}");
            std::process::exit(2);
        }
    };
    configure(tauri::Builder::default(), core)
        .run(context())
        .expect("Persephone UI failed to start");
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
