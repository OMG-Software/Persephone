//! The macOS platform layer (ADR-0011): a Keychain item for the wrapping
//! secret, `NSAlert` prompts, open and save panels and `NSWorkspace` for a
//! confirmed link on the main thread, the UI in its own process group,
//! killed with the core, the shared Unix read rules for a picked image or
//! file and the owner-only save, the system idle time from CoreGraphics, and
//! the media worker in its Seatbelt sandbox. Nothing here has been seen to
//! work with a real user, display or Keychain prompt; macOS is built and
//! tested on CI only.
//!
//! AppKit windows must be created on the process main thread, so on macOS
//! the core binary keeps that thread free: its usual work runs on a
//! `core-main` thread, and the main thread runs [`run_main_loop`]. Prompts
//! reach the main thread through [`main_thread`].

pub mod credentials;
pub mod main_loop;
pub mod pick;
pub mod ui;
pub mod vault;

pub use main_loop::{main_thread, run_main_loop};

use crate::{
    credentials::CredentialSource,
    media::{self, Encoded, ImageWorker},
    pick::ImagePicker,
    ui_host::UiHost,
    vault::KeyProtector,
};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

/// `kCGEventSourceStateCombinedSessionState` (`CGEventSourceStateID`): the
/// events of every source in the login session.
const COMBINED_SESSION_STATE: i32 = 0;
/// `kCGAnyInputEventType`, defined in `CGEventTypes.h` as
/// `((CGEventType)(~0))`: the latest keyboard, mouse or tablet input.
const ANY_INPUT_EVENT_TYPE: u32 = !0;

// CoreGraphics is a system framework; the core declares this one function
// instead of adding a binding crate (ADR-0025: no new dependency).
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// `CFTimeInterval CGEventSourceSecondsSinceLastEventType(
    /// CGEventSourceStateID, CGEventType)`: an `int32_t` and a `uint32_t`
    /// in, a `double` out.
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

/// How long this login session has had no keyboard, mouse or tablet input
/// (ADR-0025), from `CGEventSourceSecondsSinceLastEventType`. It is a
/// CoreGraphics query, not AppKit, so it is read on the idle check's own
/// thread. A value that is not a finite, non-negative number of seconds is
/// unknown (`None`), and an unknown idle time never locks.
pub(crate) fn idle_time() -> Option<std::time::Duration> {
    // SAFETY: the function takes two integers by value and returns a
    // double; it has no pointers and no other preconditions.
    let seconds = unsafe {
        CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT_TYPE)
    };
    super::idle_from_seconds(seconds)
}

/// The default profile (ADR-0013):
/// `~/Library/Application Support/OMG-Software/Persephone/profile`. A
/// missing or relative `HOME` gives `None`.
pub(crate) fn default_profile_dir() -> Option<PathBuf> {
    super::unix::absolute(env::var_os("HOME")).map(|home| {
        home.join("Library")
            .join("Application Support")
            .join("OMG-Software")
            .join("Persephone")
            .join("profile")
    })
}

/// The store key's outer layer: the shared XChaCha20-Poly1305 wrap under a
/// secret kept in the user's default keychain.
pub(crate) fn key_protector() -> Arc<dyn KeyProtector> {
    Arc::new(super::unix::Wrapped(
        vault::KeychainStore::default_keychain(),
    ))
}

pub(crate) fn credential_source() -> Box<dyn CredentialSource> {
    Box::new(credentials::NativePrompt)
}

pub(crate) fn image_picker() -> Box<dyn ImagePicker> {
    Box::new(pick::NativePanel)
}

/// Plan "macOS", Prompts and picker: the same read rules as Linux.
pub(crate) fn read_image_file(
    path: &Path,
    budgets: &ResourceBudgets,
) -> Result<Encoded, ErrorCode> {
    super::unix::read_image_file(path, budgets)
}

/// Saving a file (ADR-0029): see `unix::create_private_file`.
pub(crate) fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    super::unix::create_private_file(path)
}

/// Saving a file (ADR-0029): see `unix::replace_file`.
pub(crate) fn replace_file(partial: &Path, path: &Path) -> std::io::Result<()> {
    super::unix::replace_file(partial, path)
}

/// The contained media worker next to the core: the broker's rlimit
/// launcher and the worker's pure-computation sandbox, confirmed with
/// `sandbox_check` before input is sent (ADR-0011).
pub(crate) fn image_worker() -> Arc<dyn ImageWorker> {
    Arc::new(media::contained::ContainedWorker::next_to_core())
}

/// Kill-on-exit, no memory cap (accepted residual risk, ADR-0011).
pub(crate) fn ui_host() -> Box<dyn UiHost> {
    Box::new(ui::GroupHost)
}

/// Starts a new core for a profile switch. It is not in the UI's process
/// group, so ending the old UI does not end it.
pub(crate) fn start_detached(command: &mut std::process::Command) -> std::io::Result<()> {
    command.spawn().map(drop)
}
