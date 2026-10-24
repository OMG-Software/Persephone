//! The macOS platform layer (ADR-0011): a Keychain item for the wrapping
//! secret, `NSAlert` prompts and an `NSOpenPanel` on the main thread, and
//! the UI in its own process group, killed with the core. Nothing here has
//! been seen to work with a real user, display or Keychain prompt; macOS is
//! built and tested on CI only.
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
use std::{path::Path, sync::Arc};

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
