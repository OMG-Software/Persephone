//! The default profile directory (ADR-0013):
//! `%LOCALAPPDATA%\OMG-Software\Persephone\profile`.
//!
//! The base comes from the known-folder API, not the environment. It is the
//! local, not the roaming, application data: the stores hold this device's
//! Matrix keys under a DPAPI layer, and a roaming copy would run the same
//! device on two computers. The folder is neither the per-user install
//! directory (`%LOCALAPPDATA%\Persephone`) nor the web view's data directory,
//! which the uninstaller may delete.

use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf, ptr::null_mut};
use windows_sys::{
    Win32::{
        Foundation::S_OK,
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    },
    core::PWSTR,
};

/// The default profile, or `None` when Windows gives no absolute local
/// application data folder.
pub fn default_dir() -> Option<PathBuf> {
    local_app_data().map(|base| base.join("OMG-Software").join("Persephone").join("profile"))
}

fn local_app_data() -> Option<PathBuf> {
    let mut path: PWSTR = null_mut();
    // SAFETY: the GUID is a valid known folder, a null token means the
    // current user, and `path` receives a CoTaskMemAlloc string that is freed
    // below whether the call succeeds or not.
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DEFAULT as u32,
            null_mut(),
            &mut path,
        )
    };
    let base = (result == S_OK && !path.is_null()).then(|| {
        let mut length = 0;
        // SAFETY: on success `path` is a NUL-terminated wide string.
        unsafe {
            while *path.add(length) != 0 {
                length += 1;
            }
            PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(
                path, length,
            )))
        }
    });
    // SAFETY: `path` is null or the allocation the call returned, freed once.
    unsafe { CoTaskMemFree(path.cast()) };
    base.filter(|base| base.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN the current user WHEN the default profile is looked up THEN it
    /// is the fixed folder under the local application data, not the
    /// install directory.
    #[test]
    fn given_the_current_user_then_the_default_profile_is_under_local_app_data() {
        let dir = default_dir().expect("a local application data folder");
        assert!(dir.is_absolute());
        assert!(dir.ends_with(r"OMG-Software\Persephone\profile"));
        let base = std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA is set");
        assert_eq!(
            dir.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent()),
            Some(PathBuf::from(base).as_path())
        );
    }
}
