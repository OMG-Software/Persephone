//! Capture protection: hides the main window from screenshots, screen
//! recording and screen sharing through the Windows capture APIs (ADR-0045).
//!
//! [`CaptureState`] holds the wanted state and calls an applier when it
//! changes. It has no platform code, so tests replace the applier. Protection
//! fails safe: it is on at start, and only a successful read or save of the
//! account settings turns it off. A new session turns it on again, and a
//! settings reply for a call that started before that change is ignored.
//!
//! Only the top-level window is covered. Windows does not let a process set
//! the affinity of another process's windows, and the web view's child
//! windows belong to the web view process.

use std::sync::{Mutex, MutexGuard};

/// Applies the wanted state to the window. Called with the state locked, so
/// calls never overlap or reorder.
pub type Applier = Box<dyn Fn(bool) + Send + Sync>;

struct Inner {
    wanted: bool,
    /// Counts session changes, so a reply from an older session is ignored.
    session: u64,
    applier: Option<Applier>,
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
                applier: None,
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

    /// Sets what applies the state to the window, and applies the wanted
    /// state at once.
    pub fn set_applier(&self, applier: Applier) {
        let mut inner = self.inner();
        applier(inner.wanted);
        inner.applier = Some(applier);
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
        if let Some(applier) = &inner.applier {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
