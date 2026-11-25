//! The Windows screen source (ADR-0053): the core's own window names each
//! monitor, and the user shares one or cancels.
//!
//! The core names the monitor to the worker by its index in display order
//! (`EnumDisplayMonitors`) and its size in pixels. The worker reads its own
//! list when the share starts and refuses another size at that index. This
//! module keeps the same list rules as `crates/call-worker/src/screen/wgc.rs`
//! (at most 16 monitors, each side 1 to 16,384 pixels, the size of the
//! current display mode).
//!
//! The window is a message box: Yes shares the shown screen, No shows the
//! next one (a single screen has only Yes and Cancel), Cancel stops. A
//! message box cannot be closed from another thread, so a call that ends
//! while it is open leaves it until the user answers; the answer is then
//! dropped.

use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use persephone_call_protocol::ScreenTarget;
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDNO, IDYES, MB_DEFBUTTON1, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    MB_YESNOCANCEL, MessageBoxW,
};

use crate::calls::screen::{ScreenGrant, ScreenRefusal, ScreenSource};

/// The most monitors the list keeps (as in the worker).
const MAX_MONITORS: usize = 16;
/// The largest side of a monitor the list keeps, in pixels (as in the
/// worker).
const MAX_SIDE: u32 = 16_384;

/// One monitor, as the user sees it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
}

/// The user's answer to one screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Answer {
    Share,
    Next,
    Cancel,
}

/// The Windows monitor source.
pub struct MonitorScreen;

impl ScreenSource for MonitorScreen {
    fn available(&self) -> bool {
        true
    }

    fn choose(&self, cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal> {
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        let screens = monitors();
        let target = pick(&screens, cancel, ask)?;
        Ok(ScreenGrant {
            target,
            remote: None,
            session: None,
            closed: None,
        })
    }
}

/// The text of the window for the screen at `index` of `count`.
pub fn prompt_text(index: usize, count: usize, screen: Screen) -> String {
    let name = index + 1;
    let (what, buttons) = if count > 1 {
        (
            format!(
                "Share Screen {name} of {count} ({}x{})?",
                screen.width, screen.height
            ),
            "Yes shares this screen. No shows the next screen. Cancel stops.",
        )
    } else {
        (
            format!("Share Screen {name} ({}x{})?", screen.width, screen.height),
            "Yes shares this screen. No cancels.",
        )
    };
    format!(
        "{what}\n\nEveryone in the call can see it. Windows draws no border around a \
         shared screen; the call bar shows that it is shared.\n\n{buttons}"
    )
}

/// Shows each screen in turn until the user shares one or stops. `ask` shows
/// one window and returns the answer. A cancel flag set while a window was
/// open ends the pick even after "Share".
pub fn pick(
    screens: &[Screen],
    cancel: &AtomicBool,
    mut ask: impl FnMut(&str, bool) -> Answer,
) -> Result<ScreenTarget, ScreenRefusal> {
    if screens.is_empty() {
        return Err(ScreenRefusal::Unavailable);
    }
    let mut index = 0;
    loop {
        let screen = screens[index];
        let text = prompt_text(index, screens.len(), screen);
        let answer = ask(&text, screens.len() > 1);
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        match answer {
            Answer::Share => {
                // The list holds at most `MAX_MONITORS`, so the index fits.
                let index = u8::try_from(index).map_err(|_| ScreenRefusal::Refused)?;
                return Ok(ScreenTarget::Monitor {
                    index,
                    width: screen.width,
                    height: screen.height,
                });
            }
            Answer::Next => index = (index + 1) % screens.len(),
            Answer::Cancel => return Err(ScreenRefusal::Cancelled),
        }
    }
}

/// Whether a monitor of this size is listed (the worker's rule).
pub fn listed(width: u32, height: u32) -> bool {
    (1..=MAX_SIDE).contains(&width) && (1..=MAX_SIDE).contains(&height)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// One message box. Yes (the first button) is the default.
fn ask(text: &str, several: bool) -> Answer {
    let text = wide(text);
    let caption = wide("Persephone: share your screen");
    let kind = if several { MB_YESNOCANCEL } else { MB_YESNO };
    // SAFETY: both strings are NUL-terminated and live for the call; the box
    // has no owner window.
    let answer = unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            kind | MB_DEFBUTTON1 | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    match answer {
        IDYES => Answer::Share,
        IDNO if several => Answer::Next,
        _ => Answer::Cancel,
    }
}

struct Collector {
    found: Vec<Screen>,
}

unsafe extern "system" fn collect(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> i32 {
    // SAFETY: `data` is the `&mut Collector` that `monitors` passed and keeps
    // alive for the whole enumeration.
    let collector = unsafe { &mut *(data as *mut Collector) };
    if let Some((width, height)) = monitor_size(monitor)
        && collector.found.len() < MAX_MONITORS
        && listed(width, height)
    {
        collector.found.push(Screen { width, height });
    }
    1
}

/// The size of a monitor in pixels: its current display mode, else its
/// rectangle.
fn monitor_size(monitor: HMONITOR) -> Option<(u32, u32)> {
    // SAFETY: both structures are plain data and are zeroed before use.
    unsafe {
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) == 0 {
            return None;
        }
        let mut mode: DEVMODEW = std::mem::zeroed();
        mode.dmSize = size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0
            && mode.dmPelsWidth > 0
            && mode.dmPelsHeight > 0
        {
            return Some((mode.dmPelsWidth, mode.dmPelsHeight));
        }
        let rect = info.monitorInfo.rcMonitor;
        let width = u32::try_from(rect.right.checked_sub(rect.left)?).ok()?;
        let height = u32::try_from(rect.bottom.checked_sub(rect.top)?).ok()?;
        Some((width, height))
    }
}

/// The monitors in display order.
fn monitors() -> Vec<Screen> {
    let mut collector = Collector { found: Vec::new() };
    // SAFETY: the callback only uses the collector during the call.
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            std::ptr::null(),
            Some(collect),
            (&raw mut collector) as LPARAM,
        );
    }
    collector.found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const HD: Screen = Screen {
        width: 1920,
        height: 1080,
    };
    const QHD: Screen = Screen {
        width: 2560,
        height: 1440,
    };

    fn scripted(answers: Vec<Answer>) -> impl FnMut(&str, bool) -> Answer {
        let answers = RefCell::new(answers.into_iter());
        move |_, _| answers.borrow_mut().next().expect("an answer")
    }

    /// GIVEN two screens WHEN the user skips the first and shares the second
    /// THEN the target names index 1 with its size.
    #[test]
    fn given_two_screens_when_the_second_is_shared_then_it_is_named_by_index_and_size() {
        let cancel = AtomicBool::new(false);
        let target = pick(
            &[HD, QHD],
            &cancel,
            scripted(vec![Answer::Next, Answer::Share]),
        );
        assert_eq!(
            target,
            Ok(ScreenTarget::Monitor {
                index: 1,
                width: 2560,
                height: 1440
            })
        );
    }

    /// GIVEN screens WHEN the user cancels THEN the share stays off and no
    /// target exists.
    #[test]
    fn given_a_cancel_then_no_screen_is_picked() {
        let cancel = AtomicBool::new(false);
        assert_eq!(
            pick(&[HD], &cancel, scripted(vec![Answer::Cancel])),
            Err(ScreenRefusal::Cancelled)
        );
    }

    /// GIVEN several screens WHEN the user goes past the last THEN the list
    /// starts again at the first.
    #[test]
    fn given_the_last_screen_when_next_then_the_first_shows_again() {
        let cancel = AtomicBool::new(false);
        let target = pick(
            &[HD, QHD],
            &cancel,
            scripted(vec![Answer::Next, Answer::Next, Answer::Share]),
        );
        assert_eq!(
            target,
            Ok(ScreenTarget::Monitor {
                index: 0,
                width: 1920,
                height: 1080
            })
        );
    }

    /// GIVEN a call that ends while the window is open WHEN the user answers
    /// "Share" THEN nothing is shared.
    #[test]
    fn given_the_cancel_flag_set_while_asking_then_a_share_is_dropped() {
        let cancel = AtomicBool::new(false);
        let target = pick(&[HD], &cancel, |_, _| {
            cancel.store(true, Ordering::SeqCst);
            Answer::Share
        });
        assert_eq!(target, Err(ScreenRefusal::Cancelled));
    }

    /// GIVEN no monitor THEN screen sharing is unavailable and no window
    /// opens.
    #[test]
    fn given_no_monitor_then_the_share_is_unavailable() {
        let cancel = AtomicBool::new(false);
        let target = pick(&[], &cancel, |_, _| panic!("no window"));
        assert_eq!(target, Err(ScreenRefusal::Unavailable));
    }

    /// GIVEN the window text THEN it names the screen and its size, says that
    /// everyone sees it and that there is no border, and the buttons match
    /// the number of screens.
    #[test]
    fn given_the_prompt_then_it_names_the_screen_the_audience_and_the_buttons() {
        let one = prompt_text(0, 1, QHD);
        assert!(one.contains("Screen 1 (2560x1440)"));
        assert!(one.contains("Everyone in the call"));
        assert!(one.contains("no border"));
        assert!(one.contains("No cancels"));
        let two = prompt_text(1, 2, HD);
        assert!(two.contains("Screen 2 of 2 (1920x1080)"));
        assert!(two.contains("No shows the next screen"));
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
}
