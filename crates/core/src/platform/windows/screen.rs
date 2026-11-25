//! The Windows screen source (ADR-0053): the core's own window names each
//! monitor, and the user shares one or cancels.
//!
//! The core names the monitor to the worker by its GDI device name
//! (`MONITORINFOEXW::szDevice`, such as `\\.\DISPLAY1`), its size in pixels
//! when the user saw it, and its rectangle in physical pixels (read with
//! per-monitor DPI awareness, as the worker reads it). The worker copies
//! exactly that rectangle, so it copies the part of the desktop the user
//! approved even if a name moved. A device name names a display output, and
//! Windows can give it to another output when the monitors change, so the
//! core reads every monitor's name, rectangle and mode before the first
//! question and again after a Share: any change voids the answer, and the
//! share stays off. The worker takes only the monitor with that name, size
//! and rectangle, and checks its place again before and after each copy, so
//! a change after the answer gives "not found", ends the share, or copies
//! only the approved rectangle. The question says where each monitor is: the
//! main screen, or its side of the main screen, with its order when two or
//! more offered monitors are on one side, so monitors of the same size read
//! apart. This module lists monitors with the same rules as
//! `crates/call-worker/src/screen/wgc.rs` (the call protocol's
//! `MAX_SCREEN_MONITORS` and `MAX_MONITOR_SIDE`, the size of the current
//! display mode, in display order). A monitor whose name is not printable
//! ASCII, or has a space, is not listed. It offers only the monitors that the worker's GDI
//! capture takes (`screen_capture_size_ok`); another one is not asked, and
//! the question says how many were left out.
//!
//! The window is a message box: Yes shares the shown screen, No shows the
//! next one (a single screen has only Yes and No), Cancel stops. A thread
//! timer on the asking thread looks at the cancel flag ten times a second;
//! when the call ends, it ends the box's message loop with a quit message
//! (the way Windows documents to time out a message box), and the answer is
//! a cancel.

use std::cell::{Cell, RefCell};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use persephone_call_protocol::{
    DesktopRect, MAX_MONITOR_SIDE, MAX_SCREEN_MONITORS, MonitorName, ScreenTarget,
    screen_capture_size_ok,
};
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDNO, IDYES, KillTimer, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    MB_YESNOCANCEL, MESSAGEBOX_RESULT, MONITORINFOF_PRIMARY, MSG, MessageBoxW, PM_REMOVE,
    PeekMessageW, PostQuitMessage, SetTimer, WM_QUIT,
};

use crate::calls::screen::{ScreenGrant, ScreenRefusal, ScreenSource};
use crate::calls::screen_helper::{HelperShare, wait_until_idle};

use super::screen_helper::{BrokerHelper, HelperLaunch};

/// The most monitors the list keeps (as in the worker).
const MAX_MONITORS: usize = MAX_SCREEN_MONITORS as usize;
/// How often an open question looks at the cancel flag, in milliseconds.
const CANCEL_CHECK_MS: u32 = 100;

/// One monitor, as the user sees it, with the device name and the rectangle
/// (physical pixels) the worker finds it by. `main` is Windows' main
/// (primary) monitor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Screen {
    pub name: MonitorName,
    pub width: u32,
    pub height: u32,
    pub rect: DesktopRect,
    pub main: bool,
}

/// The user's answer to one screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Answer {
    Share,
    Next,
    Cancel,
}

/// How long a new share waits for the helper of the last share to be
/// stopped and joined. Two helpers never run at once.
const HELPER_IDLE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The Windows monitor source. After the user's Yes it starts the capture
/// helper for the share (ADR-0067).
pub struct MonitorScreen {
    launcher: Arc<dyn HelperLaunch>,
}

impl MonitorScreen {
    /// The source that starts the helper executable next to the core.
    pub fn next_to_core() -> Self {
        Self::with_launcher(Arc::new(BrokerHelper::next_to_core()))
    }

    /// The source that starts helpers with `launcher`.
    pub fn with_launcher(launcher: Arc<dyn HelperLaunch>) -> Self {
        Self { launcher }
    }
}

impl ScreenSource for MonitorScreen {
    fn available(&self) -> bool {
        true
    }

    fn choose(&self, cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal> {
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        let before = monitors();
        let target = pick_unchanged(
            &before.screens,
            cancel,
            |text, several| ask(text, several, cancel),
            || monitors().layout == before.layout,
        )?;
        Ok(ScreenGrant {
            target,
            remote: None,
            session: None,
            closed: None,
            helper: None,
        })
    }

    /// Starts the capture helper for the monitor that the user approved and
    /// the layout check confirmed. The helper takes only that monitor (name,
    /// size and rectangle), and its frame pipe's read end goes into the
    /// grant for the call worker. A previous share's helper is stopped and
    /// joined first, so no two run at once. A cancel in between stops the
    /// helper again. Any failure is `Unavailable`; no other capture is tried.
    fn capture(
        &self,
        grant: ScreenGrant,
        share: u32,
        cancel: &Arc<AtomicBool>,
    ) -> Result<ScreenGrant, ScreenRefusal> {
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        if !wait_until_idle(HELPER_IDLE_WAIT) {
            return Err(ScreenRefusal::Unavailable);
        }
        let launched = self.launcher.launch(share, &grant.target)?;
        let helper =
            HelperShare::start(share, launched.process).ok_or(ScreenRefusal::Unavailable)?;
        if cancel.load(Ordering::SeqCst) {
            // Dropping the helper stops it.
            return Err(ScreenRefusal::Cancelled);
        }
        Ok(ScreenGrant {
            remote: Some(launched.frames),
            helper: Some(helper),
            ..grant
        })
    }
}

/// The first monitor that a share can take, as a target: the ignored test
/// of the real helper uses it.
#[cfg(test)]
pub fn first_capturable_target() -> Option<ScreenTarget> {
    monitors().screens.into_iter().find_map(|screen| {
        screen_capture_size_ok(screen.width, screen.height).then_some(ScreenTarget::Monitor {
            name: screen.name,
            width: screen.width,
            height: screen.height,
            rect: screen.rect,
        })
    })
}

/// The question for the monitor at `place` (from 0) among the `count`
/// offered ones. The number the user reads counts the offered monitors, and
/// `whereabouts` (from [`whereabouts`]) says where the monitor is.
/// `left_out` monitors are listed but have a size that cannot be shared.
pub fn prompt_text(
    place: usize,
    count: usize,
    screen: Screen,
    whereabouts: Option<&str>,
    left_out: usize,
) -> String {
    let name = place + 1;
    let size = match whereabouts {
        Some(words) => format!("{}x{}, {words}", screen.width, screen.height),
        None => format!("{}x{}", screen.width, screen.height),
    };
    let (what, buttons) = if count > 1 {
        (
            format!("Share Screen {name} of {count} ({size})?"),
            "Yes shares this screen. No shows the next screen. Cancel stops.",
        )
    } else {
        (
            format!("Share Screen {name} ({size})?"),
            "Yes shares this screen. No cancels.",
        )
    };
    let left_out = match left_out {
        0 => String::new(),
        1 => "\n\nOne screen cannot be shared at its size and is not shown.".to_owned(),
        n => format!("\n\n{n} screens cannot be shared at their size and are not shown."),
    };
    // A GDI copy was not measured against the display affinity that hides
    // the main window (ADR-0045), so the question warns as on macOS.
    format!(
        "{what}\n\nEveryone in the call can see it. It can show Persephone's \
         messages. Windows draws no border around a shared screen; the call bar \
         shows that it is shared.{left_out}\n\n{buttons}"
    )
}

/// Where `screen` is: the main screen, or its side of `main` (from the
/// rectangles; a screen beside the main one on both axes is "above and
/// left" and so on). `None` when the main screen is not listed, or for a
/// rectangle that overlaps the main one (Windows does not place monitors so).
fn side(screen: Screen, main: Option<Screen>) -> Option<&'static str> {
    if screen.main {
        return Some("main screen");
    }
    let (left, top, right, bottom) = screen.rect.edges();
    let (main_left, main_top, main_right, main_bottom) = main?.rect.edges();
    let across = if right <= main_left {
        -1
    } else if left >= main_right {
        1
    } else {
        0
    };
    let down = if bottom <= main_top {
        -1
    } else if top >= main_bottom {
        1
    } else {
        0
    };
    match (across, down) {
        (-1, 0) => Some("left of the main screen"),
        (1, 0) => Some("right of the main screen"),
        (0, -1) => Some("above the main screen"),
        (0, 1) => Some("below the main screen"),
        (-1, -1) => Some("above and left of the main screen"),
        (1, -1) => Some("above and right of the main screen"),
        (-1, 1) => Some("below and left of the main screen"),
        (1, 1) => Some("below and right of the main screen"),
        _ => None,
    }
}

/// `n` as an English ordinal: 1st, 2nd, 3rd, 4th, 11th, 21st.
fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Twice the centre of a screen's rectangle (no rounding).
fn centre(screen: &Screen) -> (i64, i64) {
    let (left, top, right, bottom) = screen.rect.edges();
    (
        i64::from(left) + i64::from(right),
        i64::from(top) + i64::from(bottom),
    )
}

/// `screen`'s order in `group` along the axis on which the group's centres
/// are farther apart: "2nd from the left" or "1st from the top".
fn order(group: &[Screen], screen: Screen) -> String {
    let centres: Vec<(i64, i64)> = group.iter().map(centre).collect();
    let spread = |axis: fn(&(i64, i64)) -> i64| {
        let max = centres.iter().map(axis).max().unwrap_or(0);
        let min = centres.iter().map(axis).min().unwrap_or(0);
        max - min
    };
    let across = spread(|c| c.0) >= spread(|c| c.1);
    let (x, y) = centre(&screen);
    let before = centres
        .iter()
        .filter(|&&(cx, cy)| {
            if across {
                (cx, cy) < (x, y)
            } else {
                (cy, cx) < (y, x)
            }
        })
        .count();
    let from = if across { "left" } else { "top" };
    format!("{} from the {from}", ordinal(before + 1))
}

/// What the question says about where each `offered` screen is (same
/// order): its [`side`], and when two or more offered screens have the same
/// side, also its [`order`] among them, so screens of the same size read
/// apart. `main` is the main screen when the list has it.
pub fn whereabouts(offered: &[Screen], main: Option<Screen>) -> Vec<Option<String>> {
    let sides: Vec<Option<&'static str>> =
        offered.iter().map(|screen| side(*screen, main)).collect();
    offered
        .iter()
        .zip(&sides)
        .map(|(screen, own)| {
            let own = (*own)?;
            let group: Vec<Screen> = offered
                .iter()
                .zip(&sides)
                .filter(|(_, other)| **other == Some(own))
                .map(|(other, _)| *other)
                .collect();
            if group.len() < 2 {
                Some(own.to_owned())
            } else {
                Some(format!("{own}, {}", order(&group, *screen)))
            }
        })
        .collect()
}

/// Shows each screen that can be shared in turn until the user shares one or
/// stops. `screens` is the whole list in the worker's order; the target
/// names the chosen screen by its device name, size and rectangle. `ask`
/// shows one window and returns the answer. A cancel flag set while a window
/// was open ends the pick even after "Share".
pub fn pick(
    screens: &[Screen],
    cancel: &AtomicBool,
    mut ask: impl FnMut(&str, bool) -> Answer,
) -> Result<ScreenTarget, ScreenRefusal> {
    let offered: Vec<Screen> = screens
        .iter()
        .copied()
        .filter(|screen| screen_capture_size_ok(screen.width, screen.height))
        .collect();
    if offered.is_empty() {
        return Err(ScreenRefusal::Unavailable);
    }
    let left_out = screens.len() - offered.len();
    let main = screens.iter().copied().find(|screen| screen.main);
    let places = whereabouts(&offered, main);
    let mut at = 0;
    loop {
        let screen = offered[at];
        let text = prompt_text(at, offered.len(), screen, places[at].as_deref(), left_out);
        let answer = ask(&text, offered.len() > 1);
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        match answer {
            Answer::Share => {
                return Ok(ScreenTarget::Monitor {
                    name: screen.name,
                    width: screen.width,
                    height: screen.height,
                    rect: screen.rect,
                });
            }
            Answer::Next => at = (at + 1) % offered.len(),
            Answer::Cancel => return Err(ScreenRefusal::Cancelled),
        }
    }
}

/// [`pick`], then the check that the monitors are as they were before the
/// first question (`unchanged`, read after a Share only). A device name names
/// a display output, and Windows can give a name to another output when the
/// monitors change, so a Share given while they changed is void: the share
/// stays off ([`ScreenRefusal::Cancelled`]). The worker reads the monitor
/// again when it opens it and before and after each copy.
pub fn pick_unchanged(
    screens: &[Screen],
    cancel: &AtomicBool,
    ask: impl FnMut(&str, bool) -> Answer,
    unchanged: impl FnOnce() -> bool,
) -> Result<ScreenTarget, ScreenRefusal> {
    let target = pick(screens, cancel, ask)?;
    if unchanged() {
        Ok(target)
    } else {
        Err(ScreenRefusal::Cancelled)
    }
}

/// Whether a monitor of this size is listed (the worker's list rule). A
/// listed monitor is offered only when the capture takes its size.
pub fn listed(width: u32, height: u32) -> bool {
    (1..=MAX_MONITOR_SIDE).contains(&width) && (1..=MAX_MONITOR_SIDE).contains(&height)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

thread_local! {
    /// The cancel flag of the question open on this thread, for
    /// [`watch_cancel`].
    static OPEN_QUESTION: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    /// Whether [`watch_cancel`] ended the open question's loop.
    static QUIT_POSTED: Cell<bool> = const { Cell::new(false) };
}

/// The thread timer of an open question. The message box's own loop calls it
/// on the asking thread. When the call ended, a quit message ends that loop
/// (once); the box closes and `ask` takes the quit message back.
unsafe extern "system" fn watch_cancel(_window: HWND, _message: u32, _timer: usize, _time: u32) {
    let cancelled = OPEN_QUESTION.with(|open| {
        open.borrow()
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::SeqCst))
    });
    if cancelled && !QUIT_POSTED.replace(true) {
        // SAFETY: posts WM_QUIT to this thread's own queue.
        unsafe { PostQuitMessage(0) };
    }
}

/// One message box. No (the second button) is the default, like the core's
/// other questions, so an Enter press never shares a screen. The box closes
/// within about [`CANCEL_CHECK_MS`] when `cancel` is set, and the answer is
/// then [`Answer::Cancel`].
fn ask(text: &str, several: bool, cancel: &Arc<AtomicBool>) -> Answer {
    let text = wide(text);
    let caption = wide("Persephone: share your screen");
    let kind = if several { MB_YESNOCANCEL } else { MB_YESNO };
    ask_with_timer(
        cancel,
        several,
        // SAFETY: a thread timer with no window; `watch_cancel` matches the
        // TIMERPROC signature.
        || unsafe { SetTimer(null_mut(), 0, CANCEL_CHECK_MS, Some(watch_cancel)) },
        // SAFETY: both strings are NUL-terminated and live for the call; the
        // box has no owner window.
        || unsafe {
            MessageBoxW(
                null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                kind | MB_DEFBUTTON2 | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
            )
        },
    )
}

/// Shows one question only while its cancel timer runs. `start_timer` makes
/// the thread timer and returns its ID (0 when it failed); `show` opens the
/// box and returns the button. Without a timer, the end of the call could
/// not close the box, so no box opens and the answer is [`Answer::Cancel`]:
/// the share stays off.
fn ask_with_timer(
    cancel: &Arc<AtomicBool>,
    several: bool,
    start_timer: impl FnOnce() -> usize,
    show: impl FnOnce() -> MESSAGEBOX_RESULT,
) -> Answer {
    OPEN_QUESTION.with(|open| *open.borrow_mut() = Some(cancel.clone()));
    QUIT_POSTED.set(false);
    let timer = start_timer();
    if timer == 0 {
        OPEN_QUESTION.with(|open| open.borrow_mut().take());
        return Answer::Cancel;
    }
    let answer = show();
    // SAFETY: the thread timer made above, on this thread.
    unsafe { KillTimer(null_mut(), timer) };
    OPEN_QUESTION.with(|open| open.borrow_mut().take());
    if QUIT_POSTED.replace(false) {
        // The box's loop posts the quit message again when it ends; take it
        // off this thread's queue so it ends no later loop.
        // SAFETY: `message` is plain data that the call fills.
        unsafe {
            let mut message: MSG = std::mem::zeroed();
            PeekMessageW(&mut message, null_mut(), WM_QUIT, WM_QUIT, PM_REMOVE);
        }
        return Answer::Cancel;
    }
    match answer {
        IDYES => Answer::Share,
        IDNO if several => Answer::Next,
        _ => Answer::Cancel,
    }
}

/// The most monitors a [`Layout`] keeps; it counts every one.
const MAX_SEEN: usize = 64;

/// One monitor as Windows reports it: its device name (zeros after the
/// name), its rectangle on the virtual screen, its current display mode
/// (0x0 when it has none), and whether it is the main monitor. A change of
/// the main monitor also voids a Share, because the question named it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Seen {
    device: [u16; 32],
    rect: (i32, i32, i32, i32),
    mode: (u32, u32),
    main: bool,
}

/// Every monitor as Windows reports it, also those that are not offered,
/// for the check that nothing changed while the question was open.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Layout {
    count: usize,
    seen: Vec<Seen>,
}

/// The offered list and the layout from one enumeration.
#[derive(Default)]
struct Monitors {
    screens: Vec<Screen>,
    layout: Layout,
}

unsafe extern "system" fn collect(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> i32 {
    // SAFETY: `data` is the `&mut Monitors` that `monitors` passed and keeps
    // alive for the whole enumeration.
    let found = unsafe { &mut *(data as *mut Monitors) };
    found.layout.count += 1;
    if let Some(seen) = read_monitor(monitor) {
        if found.layout.seen.len() < MAX_SEEN {
            found.layout.seen.push(seen);
        }
        if let Some(screen) = screen_of(&seen)
            && found.screens.len() < MAX_MONITORS
            && listed(screen.width, screen.height)
        {
            found.screens.push(screen);
        }
    }
    1
}

/// What Windows reports for one monitor now.
fn read_monitor(monitor: HMONITOR) -> Option<Seen> {
    // SAFETY: both structures are plain data and are zeroed before use.
    unsafe {
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) == 0 {
            return None;
        }
        // Windows does not promise what the buffer holds after the NUL.
        let mut device = [0; 32];
        let end = info
            .szDevice
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(info.szDevice.len());
        device[..end].copy_from_slice(&info.szDevice[..end]);
        let mut mode: DEVMODEW = std::mem::zeroed();
        mode.dmSize = size_of::<DEVMODEW>() as u16;
        // A name that fills the buffer has no NUL, and the mode read needs
        // one; the worker's list then uses the rectangle too.
        let mode = if device[31] == 0
            && EnumDisplaySettingsW(device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0
        {
            (mode.dmPelsWidth, mode.dmPelsHeight)
        } else {
            (0, 0)
        };
        let rect = info.monitorInfo.rcMonitor;
        Some(Seen {
            device,
            rect: (rect.left, rect.top, rect.right, rect.bottom),
            mode,
            main: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        })
    }
}

/// A monitor's device name, its size in pixels (its current display mode,
/// else its rectangle) and its rectangle. A name that is not printable ASCII
/// other than space ([`MonitorName`]), or a rectangle the call protocol does
/// not carry, is `None`.
fn screen_of(seen: &Seen) -> Option<Screen> {
    let name = MonitorName::from_wide(&seen.device)?;
    let (left, top, right, bottom) = seen.rect;
    let rect = DesktopRect::new(left, top, right, bottom)?;
    let (width, height) = match seen.mode {
        (width, height) if width > 0 && height > 0 => (width, height),
        _ => {
            let (left, top, right, bottom) = seen.rect;
            (
                u32::try_from(right.checked_sub(left)?).ok()?,
                u32::try_from(bottom.checked_sub(top)?).ok()?,
            )
        }
    };
    Some(Screen {
        name,
        width,
        height,
        rect,
        main: seen.main,
    })
}

/// The monitors in display order, with the layout they were read from. The
/// read runs with per-monitor DPI awareness, so the rectangles are physical
/// pixels like the call worker's; the thread's own awareness is put back
/// after it (the question boxes keep theirs).
fn monitors() -> Monitors {
    let mut found = Monitors::default();
    // SAFETY: changes only the calling thread's DPI awareness; a null result
    // means it did not change.
    let before =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: the callback only uses `found` during the call.
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            std::ptr::null(),
            Some(collect),
            (&raw mut found) as LPARAM,
        );
    }
    if !before.is_null() {
        // SAFETY: puts back the awareness this thread had.
        unsafe { SetThreadDpiAwarenessContext(before) };
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Screen `number`, of this size, with this rectangle.
    fn placed(number: u8, width: u32, height: u32, rect: (i32, i32, i32, i32)) -> Screen {
        let name = format!(r"\\.\DISPLAY{number}");
        let (left, top, right, bottom) = rect;
        Screen {
            name: MonitorName::new(name.as_bytes()).expect("a name"),
            width,
            height,
            rect: DesktopRect::new(left, top, right, bottom).expect("a rectangle"),
            main: false,
        }
    }

    /// The main screen `number`, of this size, at the origin.
    fn main_screen(number: u8, width: u32, height: u32) -> Screen {
        let (right, bottom) = (width as i32, height as i32);
        Screen {
            main: true,
            ..placed(number, width, height, (0, 0, right, bottom))
        }
    }

    /// Screen `number`, of this size, with a rectangle of its own: the
    /// screens stand side by side.
    fn screen(number: u8, width: u32, height: u32) -> Screen {
        let left = (i32::from(number) - 1) * 8000;
        placed(number, width, height, (left, 0, left + 1000, 1000))
    }

    fn target(screen: Screen) -> ScreenTarget {
        ScreenTarget::Monitor {
            name: screen.name,
            width: screen.width,
            height: screen.height,
            rect: screen.rect,
        }
    }

    fn hd() -> Screen {
        screen(1, 1920, 1080)
    }

    fn qhd() -> Screen {
        screen(2, 2560, 1440)
    }

    fn scripted(answers: Vec<Answer>) -> impl FnMut(&str, bool) -> Answer {
        let answers = RefCell::new(answers.into_iter());
        move |_, _| answers.borrow_mut().next().expect("an answer")
    }

    /// GIVEN two screens WHEN the user skips the first and shares the second
    /// THEN the target names the second by its device name and size.
    #[test]
    fn given_two_screens_when_the_second_is_shared_then_it_is_named_by_device_and_size() {
        let cancel = AtomicBool::new(false);
        let chosen = pick(
            &[hd(), qhd()],
            &cancel,
            scripted(vec![Answer::Next, Answer::Share]),
        );
        assert_eq!(chosen, Ok(target(qhd())));
    }

    /// GIVEN screens WHEN the user cancels THEN the share stays off and no
    /// target exists.
    #[test]
    fn given_a_cancel_then_no_screen_is_picked() {
        let cancel = AtomicBool::new(false);
        assert_eq!(
            pick(&[hd()], &cancel, scripted(vec![Answer::Cancel])),
            Err(ScreenRefusal::Cancelled)
        );
    }

    /// GIVEN several screens WHEN the user goes past the last THEN the list
    /// starts again at the first.
    #[test]
    fn given_the_last_screen_when_next_then_the_first_shows_again() {
        let cancel = AtomicBool::new(false);
        let chosen = pick(
            &[hd(), qhd()],
            &cancel,
            scripted(vec![Answer::Next, Answer::Next, Answer::Share]),
        );
        assert_eq!(chosen, Ok(target(hd())));
    }

    /// GIVEN a call that ends while the window is open WHEN the user answers
    /// "Share" THEN nothing is shared.
    #[test]
    fn given_the_cancel_flag_set_while_asking_then_a_share_is_dropped() {
        let cancel = AtomicBool::new(false);
        let chosen = pick(&[hd()], &cancel, |_, _| {
            cancel.store(true, Ordering::SeqCst);
            Answer::Share
        });
        assert_eq!(chosen, Err(ScreenRefusal::Cancelled));
    }

    /// GIVEN no monitor THEN screen sharing is unavailable and no window
    /// opens.
    #[test]
    fn given_no_monitor_then_the_share_is_unavailable() {
        let cancel = AtomicBool::new(false);
        let chosen = pick(&[], &cancel, |_, _| panic!("no window"));
        assert_eq!(chosen, Err(ScreenRefusal::Unavailable));
    }

    /// GIVEN the window text THEN it names the screen and its size, says that
    /// everyone sees it and that there is no border, and the buttons match
    /// the number of screens.
    #[test]
    fn given_the_prompt_then_it_names_the_screen_the_audience_and_the_buttons() {
        let one = prompt_text(0, 1, qhd(), Some("main screen"), 0);
        assert!(one.contains("Screen 1 (2560x1440, main screen)?"));
        assert!(one.contains("Everyone in the call"));
        assert!(one.contains("no border"));
        assert!(one.contains("No cancels"));
        assert!(one.contains("Persephone's messages"));
        assert!(!one.contains("not shown"));
        let two = prompt_text(1, 2, hd(), Some("left of the main screen"), 1);
        assert!(two.contains("Screen 2 of 2 (1920x1080, left of the main screen)?"));
        assert!(two.contains("No shows the next screen"));
        assert!(two.contains("One screen cannot be shared at its size"));
        let unplaced = prompt_text(0, 1, hd(), None, 2);
        assert!(unplaced.contains("Screen 1 (1920x1080)?"));
        assert!(unplaced.contains("2 screens cannot be shared"));
    }

    /// GIVEN screens around the main screen WHEN the question says where
    /// each is THEN the main screen is named, each other screen gets its side
    /// of the main screen (both sides when it is beside it on both axes), and
    /// no place is said when the main screen is not listed.
    #[test]
    fn given_screens_around_the_main_one_then_each_question_says_its_side() {
        let main = main_screen(1, 1920, 1080);
        let around = [
            main,
            placed(2, 1920, 1080, (-1920, 0, 0, 1080)),
            placed(3, 1920, 1080, (1920, 0, 3840, 1080)),
            placed(4, 1920, 1080, (0, -1080, 1920, 0)),
            placed(5, 1920, 1080, (0, 1080, 1920, 2160)),
            placed(6, 1920, 1080, (1920, -1080, 3840, 0)),
            placed(7, 1920, 1080, (-1920, 1080, 0, 2160)),
        ];
        let said = whereabouts(&around, Some(main));
        let said: Vec<&str> = said
            .iter()
            .map(|words| words.as_deref().expect("a place"))
            .collect();
        assert_eq!(
            said,
            [
                "main screen",
                "left of the main screen",
                "right of the main screen",
                "above the main screen",
                "below the main screen",
                "above and right of the main screen",
                "below and left of the main screen",
            ]
        );
        // A taller screen beside the main one is still beside it.
        let tall = placed(2, 1080, 1920, (1920, -400, 3000, 1520));
        assert_eq!(
            whereabouts(&[tall], Some(main)),
            [Some("right of the main screen".to_owned())]
        );
        let lone = placed(2, 1920, 1080, (1920, 0, 3840, 1080));
        assert_eq!(whereabouts(&[lone], None), [None::<String>]);
        assert_eq!(
            whereabouts(&[main, lone], None),
            [Some("main screen".to_owned()), None]
        );
    }

    /// GIVEN two or more screens on the same side of the main screen WHEN
    /// the question says where each is THEN each also gets its order among
    /// them: from the left for screens in a row, from the top for screens in
    /// a column, so two screens of the same size read apart.
    #[test]
    fn given_screens_on_one_side_when_asked_then_their_order_tells_them_apart() {
        let main = main_screen(1, 2560, 1440);
        let row = [
            main,
            placed(2, 1920, 1080, (4480, 0, 6400, 1080)),
            placed(3, 1920, 1080, (2560, 0, 4480, 1080)),
        ];
        assert_eq!(
            whereabouts(&row, Some(main)),
            [
                Some("main screen".to_owned()),
                Some("right of the main screen, 2nd from the left".to_owned()),
                Some("right of the main screen, 1st from the left".to_owned()),
            ]
        );
        let column = [
            placed(2, 1920, 1080, (2560, 540, 4480, 1620)),
            placed(3, 1920, 1080, (2560, -540, 4480, 540)),
        ];
        assert_eq!(
            whereabouts(&column, Some(main)),
            [
                Some("right of the main screen, 2nd from the top".to_owned()),
                Some("right of the main screen, 1st from the top".to_owned()),
            ]
        );
        // Screens on other sides are not numbered.
        let apart = [
            placed(2, 1920, 1080, (-1920, 0, 0, 1080)),
            placed(3, 1920, 1080, (2560, 0, 4480, 1080)),
        ];
        assert_eq!(
            whereabouts(&apart, Some(main)),
            [
                Some("left of the main screen".to_owned()),
                Some("right of the main screen".to_owned()),
            ]
        );
        assert_eq!(
            [1, 2, 3, 4, 11, 12, 13, 21, 22, 23].map(ordinal),
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "23rd"
            ]
        );
    }

    /// GIVEN two screens of the same size, the main one second in the list
    /// WHEN the user goes through the questions THEN each says where its
    /// screen is, and the main screen is found in the whole list, also when
    /// it is too large to be offered.
    #[test]
    fn given_two_screens_of_one_size_when_asked_then_the_questions_differ() {
        let main = main_screen(2, 1920, 1080);
        let left = placed(1, 1920, 1080, (-1920, 0, 0, 1080));
        let cancel = AtomicBool::new(false);
        let asked = RefCell::new(Vec::new());
        let mut answers = vec![Answer::Next, Answer::Cancel].into_iter();
        let chosen = pick(&[left, main], &cancel, |text, _| {
            asked.borrow_mut().push(text.to_owned());
            answers.next().expect("an answer")
        });
        assert_eq!(chosen, Err(ScreenRefusal::Cancelled));
        let asked = asked.borrow();
        assert!(asked[0].contains("Screen 1 of 2 (1920x1080, left of the main screen)?"));
        assert!(asked[1].contains("Screen 2 of 2 (1920x1080, main screen)?"));
        let big_main = main_screen(2, 7680, 4320);
        let right = placed(1, 1920, 1080, (7680, 0, 9600, 1080));
        let asked = RefCell::new(String::new());
        let chosen = pick(&[right, big_main], &cancel, |text, several| {
            assert!(!several);
            *asked.borrow_mut() = text.to_owned();
            Answer::Cancel
        });
        assert_eq!(chosen, Err(ScreenRefusal::Cancelled));
        assert!(
            asked
                .borrow()
                .contains("Screen 1 (1920x1080, right of the main screen)?")
        );
    }

    /// GIVEN a monitor too large to capture between two others WHEN the user
    /// goes to the next screen and shares it THEN the large one is never
    /// asked, and the target names the shared screen.
    #[test]
    fn given_a_screen_too_large_to_capture_then_it_is_skipped() {
        let wide = screen(3, 8000, 1000);
        let cancel = AtomicBool::new(false);
        let asked = RefCell::new(Vec::new());
        let mut answers = vec![Answer::Next, Answer::Share].into_iter();
        let chosen = pick(&[hd(), wide, qhd()], &cancel, |text, several| {
            assert!(several);
            asked.borrow_mut().push(text.to_owned());
            answers.next().expect("an answer")
        });
        assert_eq!(chosen, Ok(target(qhd())));
        let asked = asked.borrow();
        assert!(asked.iter().all(|text| !text.contains("8000x1000")));
        assert!(asked[1].contains("Screen 2 of 2 (2560x1440)"));
        assert!(asked[1].contains("One screen cannot be shared"));
    }

    /// GIVEN only monitors that cannot be captured WHEN a screen is picked
    /// THEN no window opens and the share is unavailable.
    #[test]
    fn given_only_screens_too_large_to_capture_then_the_share_is_unavailable() {
        let cancel = AtomicBool::new(false);
        let big = screen(1, 5120, 2880);
        let chosen = pick(&[big], &cancel, |_, _| panic!("no window"));
        assert_eq!(chosen, Err(ScreenRefusal::Unavailable));
    }

    /// GIVEN monitor sizes WHEN listed THEN the rule is the worker's:
    /// 1 to 16,384 pixels on each side.
    #[test]
    fn given_monitor_sizes_then_the_list_rule_is_the_workers() {
        assert!(listed(1, 1));
        assert!(listed(16_384, 16_384));
        assert!(!listed(0, 1080));
        assert!(!listed(1920, 0));
        assert!(!listed(16_385, 1080));
    }

    /// GIVEN two screens WHEN the user shares one THEN the target is kept
    /// only if the monitors did not change while the question was open; a
    /// change voids the Share, and a Cancel never reads the monitors again.
    #[test]
    fn given_monitors_that_changed_while_asked_when_shared_then_nothing_is_shared() {
        let cancel = AtomicBool::new(false);
        let kept = pick_unchanged(
            &[hd(), qhd()],
            &cancel,
            scripted(vec![Answer::Share]),
            || true,
        );
        assert_eq!(kept, Ok(target(hd())));
        let changed = pick_unchanged(
            &[hd(), qhd()],
            &cancel,
            scripted(vec![Answer::Next, Answer::Share]),
            || false,
        );
        assert_eq!(changed, Err(ScreenRefusal::Cancelled));
        let cancelled = pick_unchanged(&[hd()], &cancel, scripted(vec![Answer::Cancel]), || {
            panic!("the monitors were read after a cancel")
        });
        assert_eq!(cancelled, Err(ScreenRefusal::Cancelled));
    }

    /// GIVEN this machine's monitors, unchanged WHEN they are read twice THEN
    /// the layouts are equal, so an unchanged desktop never voids a Share,
    /// and each offered screen comes from a monitor in the layout.
    #[test]
    fn given_unchanged_monitors_when_read_twice_then_the_layouts_are_equal() {
        let first = monitors();
        let second = monitors();
        assert_eq!(first.layout, second.layout);
        assert_eq!(first.screens, second.screens);
        assert!(first.layout.seen.len() <= first.layout.count);
        for screen in &first.screens {
            assert!(
                first
                    .layout
                    .seen
                    .iter()
                    .any(|seen| screen_of(seen) == Some(*screen))
            );
        }
    }

    fn seen(device: &str, rect: (i32, i32, i32, i32), mode: (u32, u32)) -> Seen {
        let mut name = [0; 32];
        for (to, from) in name.iter_mut().zip(device.encode_utf16()) {
            *to = from;
        }
        Seen {
            device: name,
            rect,
            mode,
            main: false,
        }
    }

    /// GIVEN what Windows reports for a monitor WHEN it becomes a screen THEN
    /// its size is the display mode, else its rectangle, and it keeps its
    /// rectangle; a name that is not printable ASCII or has a space, or a
    /// rectangle the call protocol does not carry (reversed, too large), is
    /// not a screen.
    #[test]
    fn given_a_reported_monitor_when_read_then_its_size_is_the_mode_else_the_rectangle() {
        let scaled = seen(r"\\.\DISPLAY1", (0, 0, 1280, 720), (2560, 1440));
        assert_eq!(
            screen_of(&scaled),
            Some(placed(1, 2560, 1440, (0, 0, 1280, 720)))
        );
        let left_of_main = (-2560, 0, 0, 1440);
        let expected = Some(placed(1, 2560, 1440, left_of_main));
        let no_mode = seen(r"\\.\DISPLAY1", left_of_main, (0, 0));
        assert_eq!(screen_of(&no_mode), expected);
        let one_side = seen(r"\\.\DISPLAY1", left_of_main, (2560, 0));
        assert_eq!(screen_of(&one_side), expected);
        assert_eq!(screen_of(&seen("DISPLAY 1", (0, 0, 10, 10), (0, 0))), None);
        assert_eq!(screen_of(&seen("", (0, 0, 10, 10), (0, 0))), None);
        let reversed = seen(r"\\.\DISPLAY1", (10, 0, 0, 10), (1920, 1080));
        assert_eq!(screen_of(&reversed), None);
        let huge = seen(r"\\.\DISPLAY1", (0, 0, 20_000, 10), (1920, 1080));
        assert_eq!(screen_of(&huge), None);
    }

    /// GIVEN a cancel timer that cannot start WHEN a question would be asked
    /// THEN no box opens and the answer is a cancel, so the share stays off,
    /// and no open question is left recorded on the thread.
    #[test]
    fn given_no_cancel_timer_when_asked_then_no_box_opens_and_it_cancels() {
        let cancel = Arc::new(AtomicBool::new(false));
        let answer = ask_with_timer(
            &cancel,
            true,
            || 0,
            || panic!("a box opened without a cancel timer"),
        );
        assert_eq!(answer, Answer::Cancel);
        assert!(OPEN_QUESTION.with(|open| open.borrow().is_none()));
        assert!(!QUIT_POSTED.get());
    }

    /// GIVEN an open question WHEN the call ends THEN the box closes less
    /// than 500 ms later and the answer is a cancel; the next question on
    /// the same thread is not ended by a left-over quit message.
    ///
    /// Ignored: it shows a real message box on the desktop for about 400 ms.
    /// Run it on Windows after a change to `ask`, and record the result in
    /// `docs/evidence/screen-share-windows.md`:
    /// `cargo test -p persephone-core --lib -- --ignored open_question --nocapture`.
    #[test]
    #[ignore = "shows a real message box on the desktop"]
    fn given_an_open_question_when_the_call_ends_then_it_closes() {
        use std::time::{Duration, Instant};
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            setter.store(true, Ordering::SeqCst);
            Instant::now()
        });
        let answer = ask("Persephone test: this closes by itself.", true, &cancel);
        let closed = Instant::now();
        let ended = timer.join().expect("the timer thread");
        assert_eq!(answer, Answer::Cancel);
        // The timer looks every CANCEL_CHECK_MS (100 ms); allow for a slow
        // desktop, but not for a box that waits for the user.
        let after = closed.saturating_duration_since(ended);
        eprintln!("the box closed {after:?} after the call ended");
        assert!(after < Duration::from_millis(500), "{after:?}");
        // A second question on this thread with the flag already set closes
        // at the first timer tick too, which shows that the quit message of
        // the first one was taken off the queue (it would end this one at
        // once, before any tick, with no cancel at all).
        let again = Instant::now();
        assert_eq!(
            ask("Persephone test: second.", false, &cancel),
            Answer::Cancel
        );
        assert!(
            again.elapsed() >= Duration::from_millis(50),
            "{:?}",
            again.elapsed()
        );
    }
}
