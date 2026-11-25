//! What the core and the Windows screen helper agree on (ADR-0067): the
//! helper's command line and its exit codes.
//!
//! The core starts one helper for each share. The helper takes input from
//! the core only: this command line, and the end of its control pipe (the
//! core closes it to ask for the end). It reads no homeserver or peer data.
//! The command line holds the share number, the two inherited handle values
//! and the monitor the user approved: its GDI device name (as hexadecimal
//! bytes, so no character needs quoting), its size and its rectangle. It
//! carries no secret.
//!
//! Both sides validate it: the core builds it from a [`ScreenTarget`], and
//! the helper parses it with the same rules (`MonitorName`, `DesktopRect`,
//! [`screen_capture_size_ok`]). Anything else, in any order, is refused.

use crate::{
    DesktopRect, MAX_MONITOR_NAME, MonitorName, ScreenState, ScreenTarget, screen_capture_size_ok,
};

/// The helper was asked to end (the control pipe closed), or the worker
/// closed the frame pipe: a normal end. The share is off.
pub const EXIT_STOPPED: u32 = 0;
/// The monitor is gone, moved, or has another name, size or rectangle than
/// the one the user approved.
pub const EXIT_NOT_FOUND: u32 = 10;
/// Windows.Graphics.Capture refused or failed.
pub const EXIT_UNAVAILABLE: u32 = 11;
/// The command line or an inherited handle was not valid.
pub const EXIT_BAD_START: u32 = 12;
/// The frame pipe closed while the helper wrote to it: the worker is gone.
pub const EXIT_PIPE_CLOSED: u32 = 13;

/// The share state that an exit code of the helper means. An exit that is
/// not in the list above (a crash, a kill, an unknown code) is
/// `Unavailable`: the share fails closed.
pub fn state_for_exit(code: u32) -> ScreenState {
    match code {
        EXIT_STOPPED | EXIT_PIPE_CLOSED => ScreenState::Off,
        EXIT_NOT_FOUND => ScreenState::NotFound,
        _ => ScreenState::Unavailable,
    }
}

/// What a helper needs to start one share.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelperStart {
    /// The core's number of the share. It goes into every frame item.
    pub share: u32,
    /// The handle value of the control pipe's read end, inside the helper.
    pub control: u32,
    /// The handle value of the frame pipe's write end, inside the helper.
    pub frames: u32,
    pub name: MonitorName,
    pub width: u32,
    pub height: u32,
    pub rect: DesktopRect,
}

/// Why a command line was refused. Closed; no text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgError {
    /// Not exactly seven arguments.
    Count,
    /// An argument that is not the expected one at its place.
    Unexpected,
    Share,
    /// A handle value that is 0, or not a number.
    Handle,
    /// The control and frame handles are the same.
    SameHandle,
    Name,
    /// A size outside the capture limits.
    Size,
    Rect,
}

/// Digits only: no sign, no space, no hexadecimal prefix, at most ten.
fn number(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// An optional minus sign, then digits only.
fn edge(text: &str) -> Option<i32> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || digits.len() > 10 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl HelperStart {
    /// The helper for `target`, or `None` when the target is not a monitor
    /// the capture takes: another kind, a size over the capture limits, or
    /// a handle value of 0 or two equal handle values.
    pub fn new(share: u32, control: u32, frames: u32, target: &ScreenTarget) -> Option<Self> {
        let ScreenTarget::Monitor {
            name,
            width,
            height,
            rect,
        } = *target
        else {
            return None;
        };
        let start = Self {
            share,
            control,
            frames,
            name,
            width,
            height,
            rect,
        };
        (control != 0 && frames != 0 && control != frames && screen_capture_size_ok(width, height))
            .then_some(start)
    }

    /// The command-line arguments, without the program name.
    pub fn to_args(&self) -> Vec<String> {
        let (left, top, right, bottom) = self.rect.edges();
        let name: String = self
            .name
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        vec![
            format!("--share={}", self.share),
            format!("--control={}", self.control),
            format!("--frames={}", self.frames),
            format!("--name={name}"),
            format!("--width={}", self.width),
            format!("--height={}", self.height),
            format!("--rect={left},{top},{right},{bottom}"),
        ]
    }

    /// Parses the command-line arguments, without the program name. Each of
    /// the seven arguments must come once, in this order.
    pub fn parse<I, S>(args: I) -> Result<Self, ArgError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<S> = args.into_iter().take(8).collect();
        if args.len() != 7 {
            return Err(ArgError::Count);
        }
        let value = |at: usize, key: &str| -> Result<&str, ArgError> {
            args[at]
                .as_ref()
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
                .ok_or(ArgError::Unexpected)
        };
        let share = number(value(0, "--share")?).ok_or(ArgError::Share)?;
        let control = number(value(1, "--control")?)
            .filter(|&handle| handle != 0)
            .ok_or(ArgError::Handle)?;
        let frames = number(value(2, "--frames")?)
            .filter(|&handle| handle != 0)
            .ok_or(ArgError::Handle)?;
        if control == frames {
            return Err(ArgError::SameHandle);
        }
        let hex = value(3, "--name")?;
        if hex.is_empty() || hex.len() > MAX_MONITOR_NAME * 2 || hex.len() % 2 != 0 {
            return Err(ArgError::Name);
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().chunks(2) {
            // Two hexadecimal digits: `from_str_radix` alone would take a sign.
            if !pair.iter().all(u8::is_ascii_hexdigit) {
                return Err(ArgError::Name);
            }
            let text = std::str::from_utf8(pair).map_err(|_| ArgError::Name)?;
            bytes.push(u8::from_str_radix(text, 16).map_err(|_| ArgError::Name)?);
        }
        let name = MonitorName::new(&bytes).ok_or(ArgError::Name)?;
        let width = number(value(4, "--width")?).ok_or(ArgError::Size)?;
        let height = number(value(5, "--height")?).ok_or(ArgError::Size)?;
        if !screen_capture_size_ok(width, height) {
            return Err(ArgError::Size);
        }
        let edges: Vec<&str> = value(6, "--rect")?.split(',').collect();
        let [left, top, right, bottom] = edges[..] else {
            return Err(ArgError::Rect);
        };
        let rect = match (edge(left), edge(top), edge(right), edge(bottom)) {
            (Some(left), Some(top), Some(right), Some(bottom)) => {
                DesktopRect::new(left, top, right, bottom).ok_or(ArgError::Rect)?
            }
            _ => return Err(ArgError::Rect),
        };
        Ok(Self {
            share,
            control,
            frames,
            name,
            width,
            height,
            rect,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(width: u32, height: u32) -> ScreenTarget {
        ScreenTarget::Monitor {
            name: MonitorName::new(br"\\.\DISPLAY2").expect("a name"),
            width,
            height,
            rect: DesktopRect::new(-1920, -100, 0, 980).expect("a rectangle"),
        }
    }

    fn args(share: u32) -> Vec<String> {
        HelperStart::new(share, 100, 104, &monitor(1920, 1080))
            .expect("a start")
            .to_args()
    }

    /// GIVEN a start WHEN it is turned into arguments and parsed THEN it
    /// is the same start, also for a negative rectangle and the largest
    /// numbers.
    #[test]
    fn given_a_start_when_round_tripped_then_it_is_kept() {
        for share in [0, 1, u32::MAX] {
            let start = HelperStart::parse(args(share)).expect("parses");
            assert_eq!(start.share, share);
            assert_eq!((start.control, start.frames), (100, 104));
            assert_eq!(start.name.as_bytes(), br"\\.\DISPLAY2");
            assert_eq!((start.width, start.height), (1920, 1080));
            assert_eq!(start.rect.edges(), (-1920, -100, 0, 980));
            assert_eq!(start.to_args(), args(share));
        }
    }

    /// GIVEN a target that is not a capturable monitor or a bad handle
    /// WHEN a start is made THEN there is none.
    #[test]
    fn given_a_bad_target_when_a_start_is_made_then_there_is_none() {
        let display = ScreenTarget::Display {
            id: 1,
            width: 100,
            height: 100,
        };
        assert!(HelperStart::new(1, 4, 8, &ScreenTarget::Portal { node: 3 }).is_none());
        assert!(HelperStart::new(1, 4, 8, &display).is_none());
        assert!(HelperStart::new(1, 4, 8, &monitor(3841, 2161)).is_none());
        assert!(HelperStart::new(1, 0, 8, &monitor(1920, 1080)).is_none());
        assert!(HelperStart::new(1, 4, 0, &monitor(1920, 1080)).is_none());
        assert!(HelperStart::new(1, 4, 4, &monitor(1920, 1080)).is_none());
        assert!(HelperStart::new(1, 4, 8, &monitor(3840, 2160)).is_some());
    }

    /// GIVEN arguments that are missing, extra, out of order, repeated,
    /// signed, padded or out of range WHEN parsed THEN each is refused with
    /// its own error.
    #[test]
    fn given_bad_arguments_when_parsed_then_each_is_refused() {
        let good = args(7);
        let with = |at: usize, text: &str| {
            let mut changed = good.clone();
            changed[at] = text.to_owned();
            changed
        };
        let parse = |list: Vec<String>| HelperStart::parse(list);
        assert_eq!(parse(Vec::new()), Err(ArgError::Count));
        assert_eq!(parse(good[..6].to_vec()), Err(ArgError::Count));
        let mut extra = good.clone();
        extra.push("--more=1".to_owned());
        assert_eq!(parse(extra), Err(ArgError::Count));
        let mut swapped = good.clone();
        swapped.swap(1, 2);
        assert_eq!(parse(swapped), Err(ArgError::Unexpected));
        assert_eq!(parse(with(0, "--share 7")), Err(ArgError::Unexpected));
        for share in [
            "--share=",
            "--share=-1",
            "--share=+1",
            "--share=0x1",
            "--share=4294967296",
        ] {
            assert_eq!(parse(with(0, share)), Err(ArgError::Share), "{share}");
        }
        assert_eq!(parse(with(1, "--control=0")), Err(ArgError::Handle));
        assert_eq!(parse(with(2, "--frames=x")), Err(ArgError::Handle));
        assert_eq!(parse(with(2, "--frames=100")), Err(ArgError::SameHandle));
        let long = format!("--name={}", "61".repeat(33));
        for name in [
            "--name=",
            "--name=5",
            "--name=zz",
            "--name=+5",
            "--name=20",
            "--name=c3a9",
            &long,
        ] {
            assert_eq!(parse(with(3, name)), Err(ArgError::Name), "{name}");
        }
        assert_eq!(parse(with(4, "--width=7681")), Err(ArgError::Size));
        assert_eq!(parse(with(4, "--width=1")), Err(ArgError::Size));
        assert_eq!(parse(with(5, "--height=7681")), Err(ArgError::Size));
        for rect in [
            "--rect=0,0,1920",
            "--rect=0,0,1920,1080,1",
            "--rect=0,0,0,0",
            "--rect=1,1,0,0",
            "--rect=a,0,1,1",
            "--rect=0,0,99999999999,1",
            "--rect=--1,0,1,1",
            "--rect=0,0,1,17000",
        ] {
            assert_eq!(parse(with(6, rect)), Err(ArgError::Rect), "{rect}");
        }
    }

    /// GIVEN each exit code WHEN mapped THEN a stop, or a worker that
    /// closed the pipe, is off; a missing monitor is not found; and every
    /// other code (a failure, a crash status, a kill) is unavailable.
    #[test]
    fn given_exit_codes_when_mapped_then_the_share_state_follows() {
        assert_eq!(state_for_exit(EXIT_STOPPED), ScreenState::Off);
        assert_eq!(state_for_exit(EXIT_PIPE_CLOSED), ScreenState::Off);
        assert_eq!(state_for_exit(EXIT_NOT_FOUND), ScreenState::NotFound);
        for code in [
            EXIT_UNAVAILABLE,
            EXIT_BAD_START,
            1,
            101,
            0xC000_0005,
            0xC000_0409,
            1_067,
            u32::MAX,
        ] {
            assert_eq!(state_for_exit(code), ScreenState::Unavailable, "{code:#x}");
        }
    }
}
