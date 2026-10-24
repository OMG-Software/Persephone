//! The Linux platform layer (ADR-0010). Implemented: reading a picked image
//! file (the shared Unix read rules) and the contained media worker (seccomp,
//! checked by the broker before any input). Not implemented yet, and failing
//! closed: the vault (the profile stays locked), the prompts and the file
//! dialog (refused), and the UI host (the UI is not started).

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

/// Plan "Linux", Vault: a Secret Service item holding the wrapping secret.
pub(crate) fn key_protector() -> Arc<dyn KeyProtector> {
    Arc::new(vault::Unsupported)
}

/// Plan "Linux", Prompts and picker: GTK3 prompts on a `core-gtk` thread.
pub(crate) fn credential_source() -> Box<dyn CredentialSource> {
    Box::new(credentials::Unsupported)
}

/// Plan "Linux", Prompts and picker: `FileChooserNative` limited to PNG and
/// JPEG.
pub(crate) fn image_picker() -> Box<dyn ImagePicker> {
    Box::new(pick::NoPicker)
}

/// Plan "Linux", Prompts and picker: the read rules with `O_NOFOLLOW` and
/// `fstat`.
pub(crate) fn read_image_file(
    path: &Path,
    budgets: &ResourceBudgets,
) -> Result<Encoded, ErrorCode> {
    super::unix::read_image_file(path, budgets)
}

/// The contained media worker next to the core: the broker's rlimit
/// launcher and the worker's seccomp allowlist, confirmed through
/// `/proc/<pid>/status` before input is sent (ADR-0010).
pub(crate) fn image_worker() -> Arc<dyn ImageWorker> {
    Arc::new(media::contained::ContainedWorker::next_to_core())
}

/// Plan "Linux", UI launch: a systemd user scope with `MemoryMax`.
pub(crate) fn ui_host() -> Box<dyn UiHost> {
    Box::new(ui_host::Unsupported)
}
