//! The macOS platform layer. Not implemented yet: every seam fails closed
//! exactly as before the platform split. The plan's "macOS" section fills
//! each function; until then the profile stays locked, prompts and the file
//! dialog are refused, no media worker runs and the UI is not started.
//!
//! AppKit windows must be created on the process main thread, so on macOS
//! the core binary keeps that thread free: its usual work runs on a
//! `core-main` thread, and the main thread runs [`run_main_loop`]. Prompts
//! reach the main thread through [`main_thread`].

use crate::{
    credentials::{self, CredentialSource},
    media::{self, Encoded, ImageWorker},
    pick::{self, ImagePicker},
    ui_host::{self, UiHost},
    vault::{self, KeyProtector},
};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::{path::Path, process::ExitCode, sync::Arc, thread::JoinHandle};

/// Plan "macOS", Vault: a Keychain generic password holding the wrapping
/// secret.
pub(crate) fn key_protector() -> Arc<dyn KeyProtector> {
    Arc::new(vault::Unsupported)
}

/// Plan "macOS", Prompts and picker: `NSAlert` with `NSSecureTextField`, on
/// the main thread.
pub(crate) fn credential_source() -> Box<dyn CredentialSource> {
    Box::new(credentials::Unsupported)
}

/// Plan "macOS", Prompts and picker: `NSOpenPanel` limited to PNG and JPEG.
pub(crate) fn image_picker() -> Box<dyn ImagePicker> {
    Box::new(pick::NoPicker)
}

/// Plan "macOS", Prompts and picker: the same read rules as Linux.
pub(crate) fn read_image_file(_: &Path, _: &ResourceBudgets) -> Result<Encoded, ErrorCode> {
    Err(ErrorCode::Unavailable)
}

/// Plan "macOS", Media worker: the worker's `sandbox_init` profile.
pub(crate) fn image_worker() -> Arc<dyn ImageWorker> {
    Arc::new(media::NoWorker)
}

/// Plan "macOS", UI launch: kill-on-exit, no memory cap.
pub(crate) fn ui_host() -> Box<dyn UiHost> {
    Box::new(ui_host::Unsupported)
}

/// Runs `task` on the process main thread and returns its result, blocking
/// the calling thread until it finishes. Prompts and panels use this, since
/// AppKit requires its windows on the main thread.
///
/// Placeholder until plan "macOS", Prompts and picker: there is no AppKit
/// run loop yet, so `task` runs inline on the calling thread.
pub fn main_thread<R: Send>(task: impl FnOnce() -> R + Send) -> R {
    task()
}

/// Runs the main thread's loop until `core_main` (the core's usual work)
/// finishes, and returns its exit code; 101, like a panic in `main`, if it
/// panicked.
///
/// Placeholder until plan "macOS", Prompts and picker: the AppKit run loop
/// replaces this join; for now the main thread only waits.
pub fn run_main_loop(core_main: JoinHandle<ExitCode>) -> ExitCode {
    core_main.join().unwrap_or(ExitCode::from(101))
}
