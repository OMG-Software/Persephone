//! The helper's run: parse the command line, take the two inherited pipes,
//! choose the approved monitor, capture it with Windows.Graphics.Capture and
//! write pictures until the core closes the control pipe.
//!
//! Every failure ends the helper with an exit code of
//! `persephone_call_protocol::screen_helper` and, when the frame pipe is
//! open, an end item that says why. The helper never falls back to another
//! monitor or to another capture method.

use std::fs::File;
use std::io::Write;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::time::{Duration, Instant};

use persephone_call_protocol::screen_frame::{ScreenEnd, write_end, write_picture};
use persephone_call_protocol::screen_helper::{
    EXIT_BAD_START, EXIT_NOT_FOUND, EXIT_PIPE_CLOSED, EXIT_STOPPED, EXIT_UNAVAILABLE, HelperStart,
};
use persephone_call_protocol::{
    MAX_CAPTURE_PIXELS, MAX_CAPTURE_SIDE, MAX_MONITOR_SIDE, MAX_SCREEN_MONITORS,
    screen_capture_size_ok,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Pipes::PeekNamedPipe;

use super::monitor::{self, Choice, Mismatch};
use super::wgc;

// The monitor rules of `monitor.rs` and `wgc.rs` keep their own numbers,
// because the containment gate compiles those files without this crate. They
// must equal the protocol's, which the core uses to name and offer a
// monitor: a different list rule or capture rule offers a monitor that the
// helper then refuses.
const _: () = assert!(
    wgc::MAX_MONITORS == MAX_SCREEN_MONITORS as usize
        && wgc::MAX_MONITOR_SIDE == MAX_MONITOR_SIDE
        && monitor::MAX_SIDE == MAX_CAPTURE_SIDE
        && monitor::MAX_PIXELS == MAX_CAPTURE_PIXELS
        && size_rule_matches()
);

/// The helper's size rule and the protocol's agree at each edge: the 2x2
/// minimum, the side limit and the pixel limit, each just inside and just
/// outside.
const fn size_rule_matches() -> bool {
    const SIDE: u32 = MAX_CAPTURE_SIDE;
    let edges: [(u32, u32); 11] = [
        (2, 2),
        (1, 2),
        (2, 1),
        (0, 0),
        (SIDE, 2),
        (SIDE + 1, 2),
        (2, SIDE + 1),
        (3840, 2160),
        (3840, 2161),
        (2880, 2880),
        (2881, 2880),
    ];
    let mut at = 0;
    while at < edges.len() {
        let (width, height) = edges[at];
        if monitor::size_ok(width, height) != screen_capture_size_ok(width, height) {
            return false;
        }
        at += 1;
    }
    true
}

/// The shortest time between two copied pictures: 5 a second at most.
/// Frames in between are dropped before any copy.
const MIN_COPY_GAP: Duration = Duration::from_millis(200);
/// How long the loop sleeps when no picture waits.
const POLL_SLEEP: Duration = Duration::from_millis(20);

/// How the run ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    /// The core closed the control pipe.
    Stopped,
    /// The monitor is gone or changed.
    NotFound,
    /// The capture failed.
    Unavailable,
    /// The worker closed the frame pipe.
    PipeClosed,
}

impl Outcome {
    fn exit_code(self) -> u32 {
        match self {
            Self::Stopped => EXIT_STOPPED,
            Self::NotFound => EXIT_NOT_FOUND,
            Self::Unavailable => EXIT_UNAVAILABLE,
            Self::PipeClosed => EXIT_PIPE_CLOSED,
        }
    }

    /// What the end item tells the worker.
    fn end(self) -> ScreenEnd {
        match self {
            Self::Stopped | Self::PipeClosed => ScreenEnd::Stopped,
            Self::NotFound => ScreenEnd::NotFound,
            Self::Unavailable => ScreenEnd::Unavailable,
        }
    }
}

/// What a failed capture start means: a monitor that is not where the user
/// approved it, or the capture itself.
fn outcome_for_stage(stage: wgc::Stage) -> Outcome {
    match stage {
        wgc::Stage::NoMonitor | wgc::Stage::SizeChanged => Outcome::NotFound,
        _ => Outcome::Unavailable,
    }
}

/// Runs the helper and returns its exit code.
pub fn run() -> u8 {
    let code = match start_from_environment() {
        Some(start) => serve(&start),
        None => EXIT_BAD_START,
    };
    // Every code is below 256.
    u8::try_from(code).unwrap_or(u8::MAX)
}

fn start_from_environment() -> Option<HelperStart> {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .take(16)
        .map(|arg| arg.into_string().ok())
        .collect::<Option<_>>()?;
    HelperStart::parse(&args).ok()
}

/// Whether `handle` is a pipe. Anything else that arrives in the place of a
/// pipe is refused before it is read or written.
fn is_pipe(handle: HANDLE) -> bool {
    // SAFETY: only asks the kernel about a handle value; an invalid value
    // gives `FILE_TYPE_UNKNOWN`.
    unsafe { GetFileType(handle) == FILE_TYPE_PIPE }
}

fn handle_of(value: u32) -> HANDLE {
    HANDLE(value as usize as *mut core::ffi::c_void)
}

/// Whether the core still holds its end of the control pipe. The core sends
/// nothing: a closed pipe, a failed read or any byte at all is a stop.
fn control_open(control: HANDLE) -> bool {
    let mut available = 0_u32;
    // SAFETY: `control` is a pipe checked at the start; `available` is a
    // valid out value; no data is read.
    let peeked = unsafe { PeekNamedPipe(control, None, 0, None, Some(&mut available), None) };
    peeked.is_ok() && available == 0
}

fn serve(start: &HelperStart) -> u32 {
    let control = handle_of(start.control);
    let frames = handle_of(start.frames);
    if !is_pipe(control) || !is_pipe(frames) {
        return EXIT_BAD_START;
    }
    // SAFETY: the core made `frames` for this helper and lists it as
    // inherited; it is a pipe (checked), and nothing else in this process
    // owns it.
    let mut frames = unsafe { File::from_raw_handle(frames.0 as RawHandle) };
    let outcome = capture(start, control, &mut frames);
    // The worker reads the reason from the end item. A pipe that is gone
    // gives no answer, and the exit code says the same.
    let _ = write_end(&mut frames, start.share, outcome.end());
    let _ = frames.flush();
    outcome.exit_code()
}

/// Rows of `source` (`stride` bytes each) copied without their padding into
/// `target`: `width * 4` bytes a row.
fn repack(source: &[u8], stride: usize, width: usize, height: usize, target: &mut Vec<u8>) -> bool {
    let row = width.saturating_mul(4);
    let Some(needed) = row.checked_mul(height) else {
        return false;
    };
    if stride < row || source.len() < stride.saturating_mul(height) {
        return false;
    }
    target.clear();
    if target.try_reserve_exact(needed).is_err() {
        return false;
    }
    for line in 0..height {
        let from = line * stride;
        target.extend_from_slice(&source[from..from + row]);
    }
    true
}

/// Lets a picture through at most once for each [`MIN_COPY_GAP`].
struct RateGate {
    last: Option<Instant>,
}

impl RateGate {
    fn admit(&mut self, now: Instant) -> bool {
        let due = self
            .last
            .is_none_or(|at| now.saturating_duration_since(at) >= MIN_COPY_GAP);
        if due {
            self.last = Some(now);
        }
        due
    }
}

fn capture(start: &HelperStart, control: HANDLE, frames: &mut File) -> Outcome {
    // The thread reads and compares monitor rectangles in physical pixels,
    // as the core does when it names the monitor.
    let list = monitor::physical_monitors();
    let device = start.name.to_wide();
    let choice = Choice::Named {
        device: &device,
        width: start.width,
        height: start.height,
        rect: start.rect.edges(),
    };
    let chosen = match monitor::choose(&list, choice) {
        Ok(chosen) => chosen,
        Err(Mismatch::NoMonitor | Mismatch::SizeChanged) => return Outcome::NotFound,
    };
    if !monitor::size_ok(chosen.width, chosen.height) {
        return Outcome::Unavailable;
    }
    // The place every later check compares with. A monitor that changed
    // since the list was read is not taken.
    let Some(place) = wgc::monitor_place(chosen.handle) else {
        return Outcome::NotFound;
    };
    if monitor::device_name(&place.device) != device.as_slice() || place.rect != start.rect.edges()
    {
        return Outcome::NotFound;
    }
    let Ok(_com) = wgc::ComGuard::start() else {
        return Outcome::Unavailable;
    };
    let mut capture = match wgc::open_monitor(&chosen) {
        Ok(capture) => capture,
        Err(stage) => return outcome_for_stage(stage),
    };

    let mut gate = RateGate { last: None };
    // The size of the first picture. A later picture of another size means
    // that the monitor changed.
    let mut first_size: Option<(u32, u32)> = None;
    let mut buffer: Vec<u8> = Vec::new();
    let mut failure: Option<Outcome> = None;
    loop {
        if !control_open(control) {
            return Outcome::Stopped;
        }
        if capture.source_closed() || wgc::monitor_place(chosen.handle) != Some(place) {
            return Outcome::NotFound;
        }
        let mut admit = || gate.admit(Instant::now());
        let mut deliver = |width: u32, height: u32, stride: u32, bytes: &[u8]| {
            if failure.is_some() {
                return;
            }
            if !monitor::size_ok(width, height) {
                failure = Some(Outcome::Unavailable);
                return;
            }
            if *first_size.get_or_insert((width, height)) != (width, height) {
                failure = Some(Outcome::NotFound);
                return;
            }
            if !repack(
                bytes,
                stride as usize,
                width as usize,
                height as usize,
                &mut buffer,
            ) {
                failure = Some(Outcome::Unavailable);
                return;
            }
            if write_picture(frames, start.share, width, height, &buffer).is_err() {
                failure = Some(Outcome::PipeClosed);
            }
        };
        let polled = capture.poll(&mut admit, &mut deliver);
        if let Some(outcome) = failure {
            return outcome;
        }
        match polled {
            Ok(true) => {}
            Ok(false) => std::thread::sleep(POLL_SLEEP),
            Err(stage) => return outcome_for_stage(stage),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Pipes::CreatePipe;

    /// GIVEN rows with padding WHEN repacked THEN each row keeps its
    /// `width * 4` bytes and the padding is gone.
    #[test]
    fn given_padded_rows_when_repacked_then_the_padding_is_dropped() {
        // 2x3 pixels, stride 12 (4 bytes of padding a row).
        let source: Vec<u8> = (0..36).collect();
        let mut target = Vec::new();
        assert!(repack(&source, 12, 2, 3, &mut target));
        assert_eq!(
            target,
            [
                0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19, 24, 25, 26, 27, 28, 29, 30,
                31
            ]
        );
    }

    /// GIVEN a stride under a row, a source shorter than its rows or a
    /// size that overflows WHEN repacked THEN it is refused and nothing is
    /// copied out of range.
    #[test]
    fn given_a_bad_stride_or_a_short_source_when_repacked_then_it_is_refused() {
        let mut target = Vec::new();
        assert!(!repack(&[0; 64], 7, 2, 2, &mut target));
        assert!(!repack(&[0; 20], 8, 2, 3, &mut target));
        assert!(!repack(&[0; 64], 8, usize::MAX, 2, &mut target));
        assert!(!repack(&[0; 64], usize::MAX, 2, usize::MAX, &mut target));
        assert!(target.is_empty());
    }

    /// GIVEN pictures that come every 33 ms WHEN they pass the gate THEN at
    /// most 5 a second get through, and the first always does.
    #[test]
    fn given_a_fast_source_when_gated_then_at_most_five_a_second_pass() {
        let start = Instant::now();
        let mut gate = RateGate { last: None };
        let passed = (0..60)
            .filter(|tick| gate.admit(start + Duration::from_millis(33 * tick)))
            .count();
        // 60 ticks span 1.98 s: pictures at 0, 200, 400 ... ms.
        assert!((9..=11).contains(&passed), "{passed}");
        let mut gate = RateGate { last: None };
        assert!(gate.admit(start));
        assert!(!gate.admit(start + Duration::from_millis(199)));
        assert!(gate.admit(start + Duration::from_millis(200)));
        // A clock that steps back lets nothing through twice.
        assert!(!gate.admit(start));
    }

    /// GIVEN each outcome WHEN mapped THEN the exit code and the end item
    /// say the same thing: a stop and a closed frame pipe are normal, a
    /// missing monitor is not found, and the rest is unavailable.
    #[test]
    fn given_outcomes_when_mapped_then_code_and_end_item_agree() {
        assert_eq!(Outcome::Stopped.exit_code(), EXIT_STOPPED);
        assert_eq!(Outcome::PipeClosed.exit_code(), EXIT_PIPE_CLOSED);
        assert_eq!(Outcome::NotFound.exit_code(), EXIT_NOT_FOUND);
        assert_eq!(Outcome::Unavailable.exit_code(), EXIT_UNAVAILABLE);
        assert_eq!(Outcome::Stopped.end(), ScreenEnd::Stopped);
        assert_eq!(Outcome::PipeClosed.end(), ScreenEnd::Stopped);
        assert_eq!(Outcome::NotFound.end(), ScreenEnd::NotFound);
        assert_eq!(Outcome::Unavailable.end(), ScreenEnd::Unavailable);
        assert_eq!(outcome_for_stage(wgc::Stage::NoMonitor), Outcome::NotFound);
        assert_eq!(
            outcome_for_stage(wgc::Stage::SizeChanged),
            Outcome::NotFound
        );
        for stage in [
            wgc::Stage::Interop,
            wgc::Stage::Item,
            wgc::Stage::Device,
            wgc::Stage::Start,
            wgc::Stage::BadSize,
        ] {
            assert_eq!(outcome_for_stage(stage), Outcome::Unavailable, "{stage:?}");
        }
    }

    fn pipe() -> (HANDLE, HANDLE) {
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        // SAFETY: both out values are valid; the handles are closed by the
        // tests that make them.
        unsafe { CreatePipe(&mut read, &mut write, None, 0) }.expect("a pipe");
        (read, write)
    }

    /// GIVEN a pipe, a file and a handle value that is not open WHEN each
    /// is checked THEN only the pipe passes.
    #[test]
    fn given_a_file_or_a_stale_handle_when_checked_then_it_is_not_a_pipe() {
        use std::os::windows::io::AsRawHandle;
        let (read, write) = pipe();
        assert!(is_pipe(read));
        assert!(is_pipe(write));
        let file = File::open(std::env::current_exe().expect("this program")).expect("a file");
        assert!(!is_pipe(HANDLE(file.as_raw_handle())));
        assert!(!is_pipe(handle_of(0x00ff_fff0)));
        assert!(!is_pipe(HANDLE::default()));
        // SAFETY: closes the two handles made above.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(read);
            let _ = windows::Win32::Foundation::CloseHandle(write);
        }
    }

    /// GIVEN a control pipe WHEN the core's end is open, has sent a byte or
    /// is closed THEN the helper keeps running only for the open, silent
    /// pipe.
    #[test]
    fn given_the_control_pipe_when_the_core_closes_or_writes_then_it_is_a_stop() {
        let (read, write) = pipe();
        assert!(control_open(read));
        // SAFETY: `write` is the pipe's write end made above.
        let mut file = unsafe { File::from_raw_handle(write.0 as RawHandle) };
        file.write_all(&[1]).expect("a byte");
        assert!(!control_open(read));
        drop(file);
        assert!(!control_open(read));
        // SAFETY: closes the read end made above.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(read);
        }
    }
}
