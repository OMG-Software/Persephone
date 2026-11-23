//! The core's side of a screen share (ADR-0053): who picks the screen, and
//! what the user may see before a screen is shared.
//!
//! The page never names a source. A share starts when the page asks, and
//! the core asks a [`ScreenSource`] to pick one: on Linux the core shows its
//! own warning window and then runs the ScreenCast portal; Windows and macOS
//! report screen sharing as unavailable (the Windows capture does not run in
//! the worker's AppContainer, `docs/evidence/screen-share-windows.md`). A
//! cancelled or refused pick leaves the share off. It never falls back to
//! another source.
//!
//! The source runs off the call loop, on a blocking thread, because the user
//! answers in a window of the desktop. Setting the cancel flag ends the wait
//! quickly.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use persephone_call_protocol::ScreenTarget;
use persephone_contracts::CallScreen;
use tokio::sync::oneshot;

/// What the user reads before a Linux screen share starts.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const LINUX_SCREEN_WARNING_TEXT: &str = concat!(
    "The screen you share can show Persephone's messages, because Linux does ",
    "not hide this window from screen capture.\n\n",
    "If you continue, the next window lets you choose what to share. Everyone ",
    "in the call can see it.\n\n",
    "Share the screen?",
);

/// The PipeWire remote that goes to the worker over the camera channel
/// (Linux). Other platforms never have one.
#[cfg(unix)]
pub type Remote = std::os::fd::OwnedFd;
#[cfg(not(unix))]
pub type Remote = ();

/// The source types the core asks the portal for: monitors (1) and windows
/// (2). The desktop's own picker shows them.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const SOURCE_TYPES: u32 = 1 | 2;

/// The portal's cursor mode "embedded": the cursor is drawn into the
/// picture.
const CURSOR_EMBEDDED: u32 = 2;

/// The cursor mode to ask for, given the portal's `AvailableCursorModes`
/// bits: embedded where offered, otherwise none is asked and the portal's
/// default stands.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn preferred_cursor_mode(available: u32) -> Option<u32> {
    (available & CURSOR_EMBEDDED != 0).then_some(CURSOR_EMBEDDED)
}

/// The node of the one stream a share has. A portal that names none or
/// several is refused: the user chose one source.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pick_node(nodes: &[u32]) -> Result<u32, ScreenRefusal> {
    match nodes {
        [node] => Ok(*node),
        _ => Err(ScreenRefusal::Refused),
    }
}

/// Why a share did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenRefusal {
    /// The user cancelled the warning, the chooser or the portal's picker,
    /// or the call ended meanwhile. The share stays off.
    Cancelled,
    /// The OS or the portal refused.
    Refused,
    /// Screen sharing cannot run here.
    Unavailable,
}

impl ScreenRefusal {
    /// The state the page shows after this refusal.
    pub fn shown(self) -> CallScreen {
        match self {
            Self::Cancelled => CallScreen::Off,
            Self::Refused => CallScreen::Refused,
            Self::Unavailable => CallScreen::Unavailable,
        }
    }
}

/// One screen the user chose. It holds the portal session open: dropping it
/// closes the session.
pub struct ScreenGrant {
    /// What the worker captures.
    pub target: ScreenTarget,
    /// Linux: the PipeWire remote. It goes to the worker, with kind `s`,
    /// directly before the `SetScreen` command.
    pub remote: Option<Remote>,
    /// Keeps the portal session open for the whole share.
    pub session: Option<Box<dyn Send + Sync>>,
    /// Fires when the desktop closes the session (the user stopped the share
    /// from the desktop's own control).
    pub closed: Option<oneshot::Receiver<()>>,
}

impl std::fmt::Debug for ScreenGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenGrant")
            .field("target", &self.target)
            .field("remote", &self.remote.is_some())
            .field("session", &self.session.is_some())
            .finish()
    }
}

/// One platform's way to let the user pick a screen.
pub trait ScreenSource: Send + Sync {
    /// Whether this platform can share the screen at all. When it cannot,
    /// the page shows `unavailable` and no window opens.
    fn available(&self) -> bool;

    /// Asks the user, in a window the core or the desktop draws, and gives
    /// the pick. Blocks until answered; call it from a blocking task.
    /// Setting `cancel` ends the wait and closes any window.
    fn choose(&self, cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal>;
}

/// A platform without screen sharing: Windows until a capability makes the
/// capture run in the worker's container, macOS until its own decision.
pub struct Unavailable;

impl ScreenSource for Unavailable {
    fn available(&self) -> bool {
        false
    }

    fn choose(&self, _cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal> {
        Err(ScreenRefusal::Unavailable)
    }
}

/// A source that never picks anything.
pub fn unavailable() -> Arc<dyn ScreenSource> {
    Arc::new(Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// GIVEN each refusal WHEN shown THEN a cancel leaves the share off and
    /// the others show their own state.
    #[test]
    fn given_refusals_then_a_cancel_leaves_the_share_off() {
        assert_eq!(ScreenRefusal::Cancelled.shown(), CallScreen::Off);
        assert_eq!(ScreenRefusal::Refused.shown(), CallScreen::Refused);
        assert_eq!(ScreenRefusal::Unavailable.shown(), CallScreen::Unavailable);
    }

    /// GIVEN a platform without screen sharing WHEN a pick is asked THEN it
    /// is unavailable at once and no window is involved.
    #[test]
    fn given_an_unavailable_platform_then_no_screen_is_picked() {
        let source = unavailable();
        assert!(!source.available());
        let cancel = Arc::new(AtomicBool::new(false));
        assert_eq!(
            source.choose(&cancel).err(),
            Some(ScreenRefusal::Unavailable)
        );
        assert!(!cancel.load(Ordering::SeqCst));
    }

    /// GIVEN the portal's answers WHEN the source types, the cursor mode and
    /// the stream are read THEN both source types are asked for, the cursor
    /// is embedded only where offered, and exactly one stream is accepted.
    #[test]
    fn given_portal_answers_then_the_request_and_the_node_are_checked() {
        assert_eq!(SOURCE_TYPES, 3);
        assert_eq!(preferred_cursor_mode(0), None);
        assert_eq!(preferred_cursor_mode(1), None);
        assert_eq!(preferred_cursor_mode(1 | 2 | 4), Some(2));
        assert_eq!(pick_node(&[57]), Ok(57));
        assert_eq!(pick_node(&[]), Err(ScreenRefusal::Refused));
        assert_eq!(pick_node(&[1, 2]), Err(ScreenRefusal::Refused));
    }

    /// GIVEN the Linux warning THEN it says that messages can be seen and
    /// that everyone in the call sees the share.
    #[test]
    fn given_the_linux_warning_then_it_names_the_risk() {
        assert!(LINUX_SCREEN_WARNING_TEXT.contains("Persephone's messages"));
        assert!(LINUX_SCREEN_WARNING_TEXT.contains("does not hide"));
        assert!(LINUX_SCREEN_WARNING_TEXT.contains("Everyone in the call"));
    }
}
