//! The core's side of a screen share (ADR-0053): who picks the screen, and
//! what the user may see before a screen is shared.
//!
//! The page never names a source. A share starts when the page asks, and
//! the core asks a [`ScreenSource`] to pick one: on Linux the core shows its
//! own warning window and then runs the ScreenCast portal; on Windows the
//! core's own window names each monitor (`platform/windows/screen.rs`; the
//! worker captures it with GDI, `docs/evidence/screen-share-windows.md`); on
//! macOS it checks the Screen Recording permission and asks which display to
//! share (ADR-0064). A cancelled or refused pick leaves the share off. It
//! never falls back to another source.
//!
//! The source runs off the call loop, on a blocking thread, because the user
//! answers in a window of the desktop. Setting the cancel flag ends the wait
//! quickly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use persephone_call_protocol::{
    MAX_CAPTURE_SIDE, MAX_SCREEN_MONITORS, ScreenTarget, screen_capture_size_ok,
};
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

/// What the user reads before a macOS screen share starts, with the display
/// named first. The shared screen can show Persephone's messages, because
/// macOS does not hide this window from screen capture (ADR-0064).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn macos_screen_question(index: usize, width: u32, height: u32, main: bool) -> String {
    format!(
        concat!(
            "Share Screen {number} ({width} x {height}{main})?\n\n",
            "Everyone in the call can see it. It can show Persephone's ",
            "messages, because macOS does not hide this window from screen ",
            "capture.\n\n",
            "Choose No to see the next screen, or to cancel.",
        ),
        number = index + 1,
        width = width,
        height = height,
        main = if main { ", main screen" } else { "" },
    )
}

/// One display as the core reads it from the system: its CoreGraphics ID,
/// and its size in points. The share names the display by both, and the
/// worker reads them again for itself (ADR-0064).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct Display {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

/// The most displays the core reads (the call protocol's limit).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAX_DISPLAYS: usize = MAX_SCREEN_MONITORS as usize;

/// A display size, in points, that the worker takes: at least 2x2, at most
/// 7680 on a side and 3840x2160 pixels. It is the worker's own rule, from
/// the call protocol.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn display_size_ok(width: u32, height: u32) -> bool {
    screen_capture_size_ok(width, height)
}

/// A display side in points, from CoreGraphics' `double`, as whole points.
/// A value that is not finite, is under 1 or is over 7680 is `None`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn points(value: f64) -> Option<u32> {
    (value.is_finite() && (1.0..=f64::from(MAX_CAPTURE_SIDE)).contains(&value))
        .then(|| value.round() as u32)
}

/// The target for the display at `index` of `displays`: its ID and size.
/// `None` when the index is past the limit or the list, the ID is 0 (no
/// display), or the size is not one the worker takes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn display_target(displays: &[Display], index: usize) -> Option<ScreenTarget> {
    let display = displays.get(index).filter(|_| index < MAX_DISPLAYS)?;
    (display.id != 0 && display_size_ok(display.width, display.height)).then_some(
        ScreenTarget::Display {
            id: display.id,
            width: display.width,
            height: display.height,
        },
    )
}

/// What the macOS source needs from the system. A test supplies its own.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub trait DisplayShare {
    /// Whether this process may record the screen now. Never asks.
    fn allowed(&self) -> bool;
    /// Asks the system for the permission. The answer is `false` at the
    /// first ask: the user answers in System Settings.
    fn request(&self) -> bool;
    /// The active displays in `CGGetActiveDisplayList` order, and the ID of
    /// the main one.
    fn displays(&self) -> (Vec<Display>, u32);
    /// Shows a Yes or No question in the core's own window. `None`: no
    /// window could run.
    fn ask(&self, text: &str) -> Option<bool>;
}

/// The macOS pick (ADR-0064): the Screen Recording permission first, then
/// one question for each display, the main one first. "Yes" is the pick; a
/// "No" on every display, or a cancel, leaves the share off. A permission
/// that is not given is `Refused`, and the page then tells the user to allow
/// Persephone in System Settings. When no display has a size the worker
/// takes, nothing is asked and the share is `Unavailable`, not a cancel the
/// user did not make.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn choose_display(
    system: &dyn DisplayShare,
    cancel: &AtomicBool,
) -> Result<ScreenGrant, ScreenRefusal> {
    if cancel.load(Ordering::SeqCst) {
        return Err(ScreenRefusal::Cancelled);
    }
    if !system.allowed() && !system.request() {
        return Err(ScreenRefusal::Refused);
    }
    let (displays, main) = system.displays();
    if displays.is_empty() {
        return Err(ScreenRefusal::Unavailable);
    }
    let mut order: Vec<usize> = (0..displays.len().min(MAX_DISPLAYS)).collect();
    // The main display first; the others keep their order.
    order.sort_by_key(|&index| displays[index].id != main);
    let mut asked = false;
    for index in order {
        let Some(target) = display_target(&displays, index) else {
            continue;
        };
        asked = true;
        let display = displays[index];
        let text = macos_screen_question(index, display.width, display.height, display.id == main);
        let answer = system.ask(&text);
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        match answer {
            Some(true) => {
                return Ok(ScreenGrant {
                    target,
                    remote: None,
                    session: None,
                    closed: None,
                });
            }
            Some(false) => {}
            None => return Err(ScreenRefusal::Unavailable),
        }
    }
    Err(if asked {
        ScreenRefusal::Cancelled
    } else {
        ScreenRefusal::Unavailable
    })
}

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

/// A platform without screen sharing: every OS without its own source (the
/// default for a launcher that offers none).
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

    struct FakeSystem {
        allowed: bool,
        displays: Vec<Display>,
        main: u32,
        answers: std::sync::Mutex<Vec<Option<bool>>>,
        asked: std::sync::Mutex<Vec<String>>,
        requested: AtomicBool,
    }

    impl FakeSystem {
        fn new(allowed: bool, answers: Vec<Option<bool>>) -> Self {
            Self {
                allowed,
                displays: vec![
                    Display {
                        id: 7,
                        width: 1440,
                        height: 900,
                    },
                    Display {
                        id: 9,
                        width: 2560,
                        height: 1440,
                    },
                ],
                main: 9,
                answers: std::sync::Mutex::new(answers),
                asked: std::sync::Mutex::new(Vec::new()),
                requested: AtomicBool::new(false),
            }
        }
    }

    impl DisplayShare for FakeSystem {
        fn allowed(&self) -> bool {
            self.allowed
        }
        fn request(&self) -> bool {
            self.requested.store(true, Ordering::SeqCst);
            false
        }
        fn displays(&self) -> (Vec<Display>, u32) {
            (self.displays.clone(), self.main)
        }
        fn ask(&self, text: &str) -> Option<bool> {
            self.asked.lock().expect("asked").push(text.to_owned());
            self.answers.lock().expect("answers").remove(0)
        }
    }

    /// GIVEN no Screen Recording permission that the system grants WHEN a
    /// display is chosen THEN the share is refused, the system was asked
    /// once and the user saw no display question.
    #[test]
    fn given_no_permission_when_a_display_is_chosen_then_it_is_refused() {
        let system = FakeSystem::new(false, vec![]);
        let cancel = AtomicBool::new(false);
        assert_eq!(
            choose_display(&system, &cancel).err(),
            Some(ScreenRefusal::Refused)
        );
        assert!(system.requested.load(Ordering::SeqCst));
        assert!(system.asked.lock().expect("asked").is_empty());
    }

    /// GIVEN a permission that is already given WHEN a display is chosen
    /// THEN the system is not asked for it again.
    #[test]
    fn given_a_permission_when_a_display_is_chosen_then_it_is_not_asked_again() {
        let system = FakeSystem::new(true, vec![Some(true)]);
        let cancel = AtomicBool::new(false);
        assert!(choose_display(&system, &cancel).is_ok());
        assert!(!system.requested.load(Ordering::SeqCst));
    }

    /// GIVEN two displays WHEN the user says No to the main one and Yes to
    /// the other THEN the main display is asked first, and the grant names
    /// the other one by its display ID and its size.
    #[test]
    fn given_two_displays_when_the_user_picks_the_second_then_the_target_names_it() {
        let system = FakeSystem::new(true, vec![Some(false), Some(true)]);
        let cancel = AtomicBool::new(false);
        let grant = choose_display(&system, &cancel).expect("grant");
        assert_eq!(
            grant.target,
            ScreenTarget::Display {
                id: 7,
                width: 1440,
                height: 900
            }
        );
        assert!(grant.remote.is_none() && grant.session.is_none() && grant.closed.is_none());
        let asked = system.asked.lock().expect("asked");
        assert!(asked[0].contains("Screen 2 (2560 x 1440, main screen)"));
        assert!(asked[1].contains("Screen 1 (1440 x 900)"));
        assert!(asked[0].contains("Persephone's messages"));
    }

    /// GIVEN a No on every display WHEN a display is chosen THEN the share
    /// is cancelled, and a missing window is unavailable.
    #[test]
    fn given_no_on_every_display_then_it_is_cancelled() {
        let cancel = AtomicBool::new(false);
        let all_no = FakeSystem::new(true, vec![Some(false), Some(false)]);
        assert_eq!(
            choose_display(&all_no, &cancel).err(),
            Some(ScreenRefusal::Cancelled)
        );
        let no_window = FakeSystem::new(true, vec![None]);
        assert_eq!(
            choose_display(&no_window, &cancel).err(),
            Some(ScreenRefusal::Unavailable)
        );
    }

    /// GIVEN displays whose sizes the worker does not take WHEN a display is
    /// chosen THEN nothing is asked and the share is unavailable, not a
    /// cancel; a size it takes is still asked.
    #[test]
    fn given_no_display_the_worker_takes_then_the_share_is_unavailable() {
        let cancel = AtomicBool::new(false);
        let mut system = FakeSystem::new(true, vec![]);
        for display in &mut system.displays {
            display.width = 7681;
        }
        assert_eq!(
            choose_display(&system, &cancel).err(),
            Some(ScreenRefusal::Unavailable)
        );
        assert!(system.asked.lock().expect("asked").is_empty());
        let mut one = FakeSystem::new(true, vec![Some(false)]);
        one.displays[1].width = 7681;
        assert_eq!(
            choose_display(&one, &cancel).err(),
            Some(ScreenRefusal::Cancelled)
        );
        assert_eq!(one.asked.lock().expect("asked").len(), 1);
    }

    /// GIVEN a cancel flag WHEN a display is chosen THEN nothing is asked.
    #[test]
    fn given_a_cancel_then_no_display_is_asked() {
        let cancel = AtomicBool::new(true);
        let system = FakeSystem::new(true, vec![]);
        assert_eq!(
            choose_display(&system, &cancel).err(),
            Some(ScreenRefusal::Cancelled)
        );
        assert!(system.asked.lock().expect("asked").is_empty());
    }

    /// GIVEN display sides from CoreGraphics WHEN read as points THEN only a
    /// finite size in range is taken, rounded.
    #[test]
    fn given_display_sides_when_read_as_points_then_only_a_sane_size_is_taken() {
        assert_eq!(points(1512.0), Some(1512));
        assert_eq!(points(982.4), Some(982));
        assert_eq!(points(0.0), None);
        assert_eq!(points(-1.0), None);
        assert_eq!(points(f64::NAN), None);
        assert_eq!(points(f64::INFINITY), None);
        assert_eq!(points(7681.0), None);
    }

    /// GIVEN display lists WHEN a target is made THEN only a listed index
    /// within the limit, a display ID and a size the worker takes give one,
    /// and the target names the display by its ID.
    #[test]
    fn given_display_lists_when_a_target_is_made_then_the_limits_hold() {
        let list = [
            Display {
                id: 1,
                width: 1512,
                height: 982,
            },
            Display {
                id: 2,
                width: 1,
                height: 1,
            },
            Display {
                id: 0,
                width: 1512,
                height: 982,
            },
        ];
        assert_eq!(
            display_target(&list, 0),
            Some(ScreenTarget::Display {
                id: 1,
                width: 1512,
                height: 982
            })
        );
        assert_eq!(display_target(&list, 1), None);
        assert_eq!(display_target(&list, 2), None, "no display has ID 0");
        assert_eq!(display_target(&list, 3), None);
        let many: Vec<Display> = (1..=40)
            .map(|id| Display {
                id,
                width: 800,
                height: 600,
            })
            .collect();
        assert!(display_target(&many, MAX_DISPLAYS - 1).is_some());
        assert_eq!(display_target(&many, MAX_DISPLAYS), None);
        assert!(!display_size_ok(7681, 100) && !display_size_ok(5120, 2880));
    }
}
