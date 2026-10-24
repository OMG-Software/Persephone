//! Per-OS implementations behind the core's platform seams.
//!
//! The traits and the OS-independent rules stay in their own modules:
//! [`KeyProtector`](crate::vault::KeyProtector),
//! [`CredentialSource`](crate::credentials::CredentialSource),
//! [`ImagePicker`](crate::pick::ImagePicker) and the picked-file read rules,
//! [`ImageWorker`](crate::media::ImageWorker) and
//! [`UiHost`](crate::ui_host::UiHost). Each module's `platform()` constructor
//! calls the matching function of [`os`], the module for the target OS:
//!
//! * `key_protector`, `credential_source`, `image_picker`, `read_image_file`,
//!   `image_worker` and `ui_host`.
//!
//! An OS whose layer is not implemented returns the fail-closed stand-ins:
//! the vault refuses (`Unsupported`), prompts and the file dialog are
//! unsupported or unavailable, there is no media worker (`NoWorker`), and the
//! UI is not started.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux as os;
#[cfg(target_os = "macos")]
pub(crate) use macos as os;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) use other as os;
#[cfg(windows)]
pub(crate) use windows as os;

/// Any other OS: every seam fails closed.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod other {
    use crate::{
        credentials::{self, CredentialSource},
        media::{self, Encoded, ImageWorker},
        pick::{self, ImagePicker},
        ui_host::{self, UiHost},
        vault::{self, KeyProtector},
    };
    use persephone_core_protocol::ErrorCode;
    use persephone_media_protocol::ResourceBudgets;
    use std::{path::Path, sync::Arc};

    pub(crate) fn key_protector() -> Arc<dyn KeyProtector> {
        Arc::new(vault::Unsupported)
    }

    pub(crate) fn credential_source() -> Box<dyn CredentialSource> {
        Box::new(credentials::Unsupported)
    }

    pub(crate) fn image_picker() -> Box<dyn ImagePicker> {
        Box::new(pick::NoPicker)
    }

    pub(crate) fn read_image_file(_: &Path, _: &ResourceBudgets) -> Result<Encoded, ErrorCode> {
        Err(ErrorCode::Unavailable)
    }

    pub(crate) fn image_worker() -> Arc<dyn ImageWorker> {
        Arc::new(media::NoWorker)
    }

    pub(crate) fn ui_host() -> Box<dyn UiHost> {
        Box::new(ui_host::Unsupported)
    }
}
