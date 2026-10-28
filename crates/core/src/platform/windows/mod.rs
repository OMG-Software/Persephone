//! The Windows platform layer: DPAPI, CredUI prompts, the file dialog and
//! read, the contained media worker, the UI's Job, and the default profile
//! directory.

pub mod credentials;
pub mod media;
pub mod pick;
pub mod profile;
pub mod ui;
pub mod vault;

use crate::{
    credentials::CredentialSource, media::Encoded, media::ImageWorker, pick::ImagePicker,
    ui_host::UiHost, vault::KeyProtector,
};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};

/// COM on the calling thread, for shell calls, until dropped. A thread whose
/// COM is already set up in another model (`RPC_E_CHANGED_MODE`) keeps it, and
/// nothing is released.
pub(crate) struct Apartment(bool);

impl Apartment {
    pub(crate) fn enter() -> Self {
        // SAFETY: no reserved argument; balanced by `drop` on success.
        let result = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        Self(result >= 0)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

/// How long this computer has had no keyboard or mouse input, from
/// `GetLastInputInfo` (ADR-0025). Its tick count wraps every 49.7 days; the
/// difference is taken with wrapping, which is right for any idle time
/// shorter than that.
pub(crate) fn idle_time() -> Option<std::time::Duration> {
    use windows_sys::Win32::{
        System::SystemInformation::GetTickCount,
        UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
    };
    let mut info = LASTINPUTINFO {
        cbSize: u32::try_from(std::mem::size_of::<LASTINPUTINFO>()).ok()?,
        dwTime: 0,
    };
    // SAFETY: `info` is a correctly sized LASTINPUTINFO that lives for the
    // call; GetTickCount takes nothing.
    let (read, now) = unsafe { (GetLastInputInfo(&mut info), GetTickCount()) };
    (read != 0).then(|| std::time::Duration::from_millis(now.wrapping_sub(info.dwTime).into()))
}

pub(crate) fn default_profile_dir() -> Option<PathBuf> {
    profile::default_dir()
}

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
    limit: u64,
    budgets: &ResourceBudgets,
) -> Result<Encoded, ErrorCode> {
    pick::read(path, limit, budgets)
}

pub(crate) fn image_worker() -> Arc<dyn ImageWorker> {
    Arc::new(media::ContainedWorker::next_to_core())
}

pub(crate) fn ui_host() -> Box<dyn UiHost> {
    Box::new(ui::JobHost)
}

/// Starts a new core for a profile switch, detached from this one as the
/// launcher starts it: no console and its own process group, so it outlives
/// this process.
pub(crate) fn start_detached(command: &mut std::process::Command) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    command
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(drop)
}
