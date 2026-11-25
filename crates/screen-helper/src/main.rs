//! The Windows screen helper (ADR-0067).
//!
//! The core starts one helper for each screen share, outside the call
//! worker's AppContainer, because Windows.Graphics.Capture is refused there.
//! The helper holds no key, opens no network connection and reads no
//! homeserver or peer data. It takes input from the core only: its command
//! line (`persephone_call_protocol::screen_helper`) and the end of its
//! control pipe. It captures the one monitor that the user approved, and
//! writes bounded BGRA pictures, at most 5 a second, to the frame pipe that
//! the call worker reads (`persephone_call_protocol::screen_frame`). It never
//! sends a picture to the core or to the web view.
//!
//! On any other OS the program only exits with an error: Linux uses the
//! ScreenCast portal and macOS uses ScreenCaptureKit in the call worker.

// No console: the core starts the helper without one, and a console program
// would need a child process (`conhost.exe`) that its launch forbids.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

use std::process::ExitCode;

#[cfg(windows)]
mod monitor;
#[cfg(windows)]
mod run;
#[cfg(windows)]
#[allow(dead_code)] // the containment gate uses the parts the helper does not
mod wgc;

#[cfg(windows)]
fn main() -> ExitCode {
    ExitCode::from(run::run())
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    // Only Windows has a helper; nothing starts one elsewhere.
    ExitCode::from(persephone_call_protocol::screen_helper::EXIT_BAD_START as u8)
}
