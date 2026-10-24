//! The Windows platform layer: DPAPI, CredUI prompts, the file dialog and
//! read, the contained media worker, and the UI's Job.

pub mod credentials;
pub mod media;
pub mod pick;
pub mod ui;
pub mod vault;

use crate::{
    credentials::CredentialSource, media::Encoded, media::ImageWorker, pick::ImagePicker,
    ui_host::UiHost, vault::KeyProtector,
};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::{path::Path, sync::Arc};

pub(crate) fn key_protector() -> Arc<dyn KeyProtector> {
    Arc::new(vault::Dpapi::new())
}

pub(crate) fn credential_source() -> Box<dyn CredentialSource> {
    Box::new(credentials::NativePrompt)
}

pub(crate) fn image_picker() -> Box<dyn ImagePicker> {
    Box::new(pick::NativeDialog)
}

pub(crate) fn read_image_file(
    path: &Path,
    budgets: &ResourceBudgets,
) -> Result<Encoded, ErrorCode> {
    pick::read(path, budgets)
}

pub(crate) fn image_worker() -> Arc<dyn ImageWorker> {
    Arc::new(media::ContainedWorker::next_to_core())
}

pub(crate) fn ui_host() -> Box<dyn UiHost> {
    Box::new(ui::JobHost)
}
