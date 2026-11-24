//! Capture protection: hides the main window from screenshots, screen
//! recording and screen sharing through the Windows capture APIs (ADR-0045).
//!
//! [`CaptureState`] holds the wanted state and calls an applier when it
//! changes. It has no platform code, so tests replace the applier. Protection
//! fails safe: it is on at start, and only a successful read or save of the
//! account settings turns it off. A new session turns it on again, and a
//! settings reply for a call that started before that change is ignored.
//!
//! Each top-level window of the app is covered: the main window and the
//! call window (ADR-0060) each set an applier under their label.
//! Only the top-level windows are covered. Windows does not let a process set
//! the affinity of another process's windows, and the web view's child
//! windows belong to the web view process.
//!
//! Linux has no way for an app to hide a window from capture. The page only
//! says which display server draws the window ([`display_session`]), because
//! that decides who can read it (ADR-0047).

use persephone_contracts::DisplaySession;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Applies the wanted state to the window. Called with the state locked, so
/// calls never overlap or reorder.
pub type Applier = Box<dyn Fn(bool) + Send + Sync>;

struct Inner {
    wanted: bool,
    /// Counts session changes, so a reply from an older session is ignored.
    session: u64,
    /// One applier per window label.
    appliers: BTreeMap<String, Applier>,
}

/// The wanted capture protection. On by default.
pub struct CaptureState {
    inner: Mutex<Inner>,
}

impl Default for CaptureState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                wanted: true,
                session: 0,
                appliers: BTreeMap::new(),
            }),
        }
    }
}

impl CaptureState {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// True when the window should be hidden from capture.
    pub fn wanted(&self) -> bool {
        self.inner().wanted
    }

    /// Sets what applies the state to the main window, and applies the
    /// wanted state at once.
    pub fn set_applier(&self, applier: Applier) {
        self.set_window_applier("main", applier);
    }

    /// Sets what applies the state to the window `label`, replacing its
    /// earlier one, and applies the wanted state at once.
    pub fn set_window_applier(&self, label: &str, applier: Applier) {
        let mut inner = self.inner();
        applier(inner.wanted);
        inner.appliers.insert(label.to_owned(), applier);
    }

    /// The window `label` has gone: nothing is applied to it any more.
    pub fn remove_window_applier(&self, label: &str) {
        self.inner().appliers.remove(label);
    }

    /// The current session number. Take it before a settings call starts.
    pub fn session(&self) -> u64 {
        self.inner().session
    }

    /// Sets the wanted state from a successful settings reply or save, unless
    /// the session changed after `session` was taken.
    pub fn set_for(&self, session: u64, on: bool) {
        let mut inner = self.inner();
        if inner.session == session {
            Self::apply(&mut inner, on);
        }
    }

    /// Turns protection on again, as a new session starts, and makes every
    /// settings call already in flight stale.
    pub fn protect(&self) {
        let mut inner = self.inner();
        inner.session = inner.session.wrapping_add(1);
        Self::apply(&mut inner, true);
    }

    fn apply(inner: &mut Inner, on: bool) {
        inner.wanted = on;
        for applier in inner.appliers.values() {
            applier(on);
        }
    }
}

/// Applies the state to a window: excluded from capture when on, and shown
/// normally when off. Older Windows 10 builds lack the exclusion, so it falls
/// back to a black window in a capture. A failure prints one line and the
/// app carries on.
#[cfg(windows)]
pub fn apply_to_window(hwnd: isize, on: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR, WDA_NONE,
    };
    let handle = hwnd as *mut core::ffi::c_void;
    // SAFETY: the call takes a window handle and a flag. A stale or invalid
    // handle makes it fail, which is handled below.
    let done = unsafe {
        if on {
            SetWindowDisplayAffinity(handle, WDA_EXCLUDEFROMCAPTURE) != 0
                || SetWindowDisplayAffinity(handle, WDA_MONITOR) != 0
        } else {
            SetWindowDisplayAffinity(handle, WDA_NONE) != 0
        }
    };
    if !done {
        eprintln!("persephone-ui: cannot hide the window from capture");
    }
}

/// The display server GTK opened for the main window, recorded once at
/// start ([`record_display_session`]).
static DISPLAY: OnceLock<DisplaySession> = OnceLock::new();

/// The display server that draws the main window: on Linux, the GDK display
/// GTK opened; `Unknown` elsewhere, or when it could not be read.
pub fn display_session() -> DisplaySession {
    DISPLAY.get().copied().unwrap_or(DisplaySession::Unknown)
}

/// Records the display server once. A later call changes nothing.
pub fn record_display_session(session: DisplaySession) {
    let _ = DISPLAY.set(session);
}

/// The display server of a GDK display, from its type name. GTK names the
/// backend that actually opened, so a failed Wayland connection that fell
/// back to X11 (XWayland included) reads as X11. Any other backend is
/// `Unknown`.
pub fn session_of_display_type(name: &str) -> DisplaySession {
    match name {
        "GdkWaylandDisplay" => DisplaySession::Wayland,
        "GdkX11Display" => DisplaySession::X11,
        _ => DisplaySession::Unknown,
    }
}

/// Reads the display server of the window's GDK display. Must run on the
/// GTK main thread.
#[cfg(target_os = "linux")]
pub fn read_display_session(window: &tauri::WebviewWindow) -> DisplaySession {
    use gtk::prelude::{ObjectExt, WidgetExt};
    match window.gtk_window() {
        Ok(gtk_window) => {
            let display = WidgetExt::display(&gtk_window);
            session_of_display_type(ObjectExt::type_(&display).name())
        }
        Err(_) => DisplaySession::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// GIVEN the type name of the GDK display GTK opened THEN Wayland and
    /// X11 are told apart, a Wayland session where GTK fell back to X11
    /// reads as X11, and any other or unknown backend is `Unknown`.
    #[test]
    fn given_a_gdk_display_type_then_the_session_names_the_opened_backend() {
        assert_eq!(
            session_of_display_type("GdkWaylandDisplay"),
            DisplaySession::Wayland
        );
        // A Wayland desktop where the Wayland connection failed: GTK opened
        // X11 through XWayland, so any X11 client can read the window.
        assert_eq!(
            session_of_display_type("GdkX11Display"),
            DisplaySession::X11
        );
        for other in ["GdkBroadwayDisplay", "GdkDisplay", "", "gdkwaylanddisplay"] {
            assert_eq!(
                session_of_display_type(other),
                DisplaySession::Unknown,
                "{other}"
            );
        }
    }

    fn recording(state: &CaptureState) -> Arc<Mutex<Vec<bool>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        state.set_applier(Box::new(move |on| seen.lock().unwrap().push(on)));
        calls
    }

    /// GIVEN a new state WHEN an applier is set THEN protection is wanted and
    /// applied at once.
    #[test]
    fn given_a_new_state_then_protection_is_on_and_applied() {
        let state = CaptureState::default();
        assert!(state.wanted());
        let calls = recording(&state);
        assert_eq!(*calls.lock().unwrap(), vec![true]);
    }

    /// GIVEN protection set off WHEN a session starts THEN it is on again.
    #[test]
    fn given_protection_off_when_protect_then_it_is_on_again() {
        let state = CaptureState::default();
        let calls = recording(&state);
        state.set_for(state.session(), false);
        assert!(!state.wanted());
        state.protect();
        assert!(state.wanted());
        assert_eq!(*calls.lock().unwrap(), vec![true, false, true]);
    }

    /// GIVEN a settings call that started before a session change WHEN its
    /// reply says off THEN protection stays on and nothing is applied.
    #[test]
    fn given_a_reply_from_an_older_session_then_it_is_ignored() {
        let state = CaptureState::default();
        let calls = recording(&state);
        let before = state.session();
        state.protect();
        state.set_for(before, false);
        assert!(state.wanted());
        assert_eq!(*calls.lock().unwrap(), vec![true, true]);
        state.set_for(state.session(), false);
        assert!(!state.wanted());
    }

    /// GIVEN a main and a call window WHEN the state changes THEN both get
    /// it; WHEN the call window has gone THEN only the main window does.
    #[test]
    fn given_two_windows_then_each_gets_the_state_until_it_goes() {
        let state = CaptureState::default();
        let main = recording(&state);
        let call = Arc::new(Mutex::new(Vec::new()));
        let seen = call.clone();
        state.set_window_applier("call", Box::new(move |on| seen.lock().unwrap().push(on)));
        state.set_for(state.session(), false);
        state.remove_window_applier("call");
        state.protect();
        assert_eq!(*main.lock().unwrap(), vec![true, false, true]);
        assert_eq!(*call.lock().unwrap(), vec![true, false]);
    }

    /// GIVEN no applier WHEN the state changes THEN the wanted state still
    /// changes, and a later applier gets it.
    #[test]
    fn given_no_applier_then_the_wanted_state_waits_for_one() {
        let state = CaptureState::default();
        state.set_for(state.session(), false);
        assert!(!state.wanted());
        let calls = recording(&state);
        assert_eq!(*calls.lock().unwrap(), vec![false]);
    }
}
