//! Persephone UI process (ADR-0004).
//!
//! A thin, keyless Tauri host. The web view may call the commands registered
//! here, subject to the capabilities in `capabilities/`, and nothing else. Each
//! command is relayed to the key-holding core as one bounded frame; the UI holds
//! no sessions, keys or stores. The main window can navigate only within the
//! bundled app and cannot open new windows.
//!
//! The page may subscribe to changes in a room. The core sends a content-free
//! event when a subscribed room changes; the UI forwards it as an empty message
//! on the page's channel for that room, and the page fetches what it needs.
//! When the core's session locks or unlocks, every room channel is dropped
//! (the core drops those subscriptions too) and the page's session channel
//! gets `"session"`, so the page clears what it shows. When rooms are added or
//! change, the session channel gets `"rooms"` and the page fetches the list.

pub mod core_link;

use core_link::{CoreError, CoreLink};
use persephone_contracts::{AccountStatus, AppStatus, RoomList, StoreState, TimelinePage};
use persephone_core_protocol::{
    Call, ErrorCode, Event, MAX_CURSOR_BYTES, MAX_HOMESERVER_URL_BYTES, MAX_ROOM_ID_BYTES,
};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};
use tauri::{
    Manager, Runtime, State, Url, WebviewUrl, WebviewWindowBuilder,
    ipc::Channel,
    webview::{NewWindowResponse, PageLoadEvent},
};

/// The label the `main` capability is bound to.
pub const MAIN_WINDOW: &str = "main";

/// Argument the core passes when it starts the UI.
pub const STARTED_BY_CORE: &str = "--started-by-core";

/// Error strings the web view may see. Anything else stays in Rust.
const UNAVAILABLE: &str = "core_unavailable";
const INVALID_ARGUMENT: &str = "invalid_argument";
const NOT_FOUND: &str = "not_found";
const LIMIT_EXCEEDED: &str = "limit_exceeded";
const LOCKED: &str = "locked";

fn page_error(error: CoreError) -> String {
    match error {
        CoreError::Core(ErrorCode::InvalidArgument) => INVALID_ARGUMENT,
        CoreError::Core(ErrorCode::NotFound) => NOT_FOUND,
        CoreError::Core(ErrorCode::LimitExceeded) => LIMIT_EXCEEDED,
        CoreError::Core(ErrorCode::Locked) => LOCKED,
        _ => UNAVAILABLE,
    }
    .to_string()
}

/// Sends one call to the core off the UI thread and checks the reply against
/// the expected contract type, which rejects unknown fields.
async fn relay<T: DeserializeOwned + Send + 'static>(
    core: &CoreLink,
    call: Call,
) -> Result<T, String> {
    let core = core.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let value = core.call(call)?;
        serde_json::from_value::<T>(value).map_err(|_| CoreError::BadReply)
    })
    .await
    .map_err(|_| UNAVAILABLE.to_string())?
    .map_err(page_error)
}

#[tauri::command]
async fn app_status(core: State<'_, CoreLink>) -> Result<AppStatus, String> {
    relay(core.inner(), Call::AppStatus {}).await
}

#[tauri::command]
async fn room_list(core: State<'_, CoreLink>) -> Result<RoomList, String> {
    relay(core.inner(), Call::RoomList {}).await
}

/// Length checks here keep obviously bad input off the core channel; the core
/// validates authoritatively.
#[tauri::command]
async fn timeline_page(
    core: State<'_, CoreLink>,
    room_id: String,
    cursor: Option<String>,
) -> Result<TimelinePage, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES
        || cursor.as_ref().is_some_and(|c| c.len() > MAX_CURSOR_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::TimelinePage { room_id, cursor }).await
}

/// The page's channel for each room it subscribed to, and its one session
/// channel. The core accepts at most `MAX_SUBSCRIPTIONS` rooms, and a room
/// channel is kept only after the core accepted the room, so this map is
/// bounded the same way.
#[derive(Default)]
pub struct Subscriptions {
    channels: Mutex<HashMap<String, Channel<()>>>,
    session: Mutex<Option<Channel<&'static str>>>,
}

impl Subscriptions {
    fn channels(&self) -> MutexGuard<'_, HashMap<String, Channel<()>>> {
        self.channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session(&self) -> MutexGuard<'_, Option<Channel<&'static str>>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Forwards a core event to the page. Called on the core reader thread.
    /// A page that has gone away simply misses the message.
    pub fn dispatch(&self, event: Event) {
        match event {
            Event::RoomChanged { room_id } => {
                if let Some(channel) = self.channels().get(&room_id) {
                    let _ = channel.send(());
                }
            }
            Event::SessionChanged {} => {
                self.channels().clear();
                if let Some(channel) = self.session().as_ref() {
                    let _ = channel.send("session");
                }
            }
            Event::RoomListChanged {} => {
                if let Some(channel) = self.session().as_ref() {
                    let _ = channel.send("rooms");
                }
            }
        }
    }

    /// Rooms the page is currently subscribed to (for tests).
    pub fn rooms(&self) -> Vec<String> {
        let mut rooms: Vec<String> = self.channels().keys().cloned().collect();
        rooms.sort();
        rooms
    }
}

/// Subscribes the page to changes in a room. The page should fetch the
/// newest page after this returns; a change between the core accepting the
/// subscription and the channel being stored is covered by that fetch.
#[tauri::command]
async fn subscribe_room(
    core: State<'_, CoreLink>,
    subscriptions: State<'_, Arc<Subscriptions>>,
    room_id: String,
    on_change: Channel<()>,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(
        core.inner(),
        Call::Subscribe {
            room_id: room_id.clone(),
        },
    )
    .await?;
    subscriptions.channels().insert(room_id, on_change);
    Ok(())
}

#[tauri::command]
async fn unsubscribe_room(
    core: State<'_, CoreLink>,
    subscriptions: State<'_, Arc<Subscriptions>>,
    room_id: String,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    subscriptions.channels().remove(&room_id);
    relay::<()>(core.inner(), Call::Unsubscribe { room_id }).await
}

/// Registers the page's session channel, replacing any earlier one.
#[tauri::command]
fn watch_session(subscriptions: State<'_, Arc<Subscriptions>>, on_change: Channel<&'static str>) {
    *subscriptions.session() = Some(on_change);
}

/// Locks the core's session. Replies with the resulting state; `locking`
/// means admitted work is still finishing, and calling again resumes the wait.
#[tauri::command]
async fn lock_session(core: State<'_, CoreLink>) -> Result<StoreState, String> {
    relay(core.inner(), Call::LockSession {}).await
}

#[tauri::command]
async fn unlock_session(core: State<'_, CoreLink>) -> Result<StoreState, String> {
    relay(core.inner(), Call::UnlockSession {}).await
}

/// Starts signing in. Only the homeserver URL crosses: the core asks for the
/// username and password itself, in a native prompt. The core validates the
/// URL; this length check keeps oversized input off the channel.
#[tauri::command]
async fn sign_in(core: State<'_, CoreLink>, homeserver: String) -> Result<AccountStatus, String> {
    if homeserver.len() > MAX_HOMESERVER_URL_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SignIn { homeserver }).await
}

/// Signs out: the core revokes the session if it can and deletes this
/// device's local profile. Replies with the resulting store state.
#[tauri::command]
async fn sign_out(core: State<'_, CoreLink>) -> Result<StoreState, String> {
    relay(core.inner(), Call::SignOut {}).await
}

/// Drops every subscription when the page (re)loads: the new page holds none
/// of the old channels, and the core's slots would otherwise stay taken.
fn reset_subscriptions(core: &CoreLink, subscriptions: &Subscriptions) {
    subscriptions.session().take();
    let rooms: Vec<String> = subscriptions
        .channels()
        .drain()
        .map(|(room, _)| room)
        .collect();
    if rooms.is_empty() {
        return;
    }
    let core = core.clone();
    tauri::async_runtime::spawn_blocking(move || {
        for room_id in rooms {
            let _ = core.call(Call::Unsubscribe { room_id });
        }
    });
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
pub fn configure<R: Runtime>(
    builder: tauri::Builder<R>,
    core: CoreLink,
    subscriptions: Arc<Subscriptions>,
) -> tauri::Builder<R> {
    builder
        .manage(core)
        .manage(subscriptions)
        .invoke_handler(tauri::generate_handler![
            app_status,
            room_list,
            timeline_page,
            subscribe_room,
            unsubscribe_room,
            watch_session,
            lock_session,
            unlock_session,
            sign_in,
            sign_out
        ])
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                reset_subscriptions(
                    webview.state::<CoreLink>().inner(),
                    webview.state::<Arc<Subscriptions>>().inner(),
                );
            }
        })
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
    let subscriptions = Arc::new(Subscriptions::default());
    let sink = {
        let subscriptions = subscriptions.clone();
        Box::new(move |event| subscriptions.dispatch(event))
    };
    let core = match core_link::PipeTransport::from_stdio(sink) {
        Ok(link) => Arc::new(link) as CoreLink,
        Err(error) => {
            eprintln!("persephone-ui: no core channel: {error}");
            std::process::exit(2);
        }
    };
    configure(tauri::Builder::default(), core, subscriptions)
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
