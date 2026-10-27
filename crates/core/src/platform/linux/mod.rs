//! The Linux platform layer (ADR-0010). Implemented: the default profile
//! directory, reading a picked image file (the shared Unix read rules) and
//! the contained media worker (seccomp, checked by the broker before any
//! input). Not implemented yet, and failing closed: the vault (the profile
//! stays locked), the prompts and the file dialog (refused), and the UI host
//! (the UI is not started).

use crate::{
    credentials::{self, CredentialSource},
    media::{self, Encoded, ImageWorker},
    pick::{self, ImagePicker},
    ui_host::{self, UiHost},
    vault::{self, KeyProtector},
};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The default profile (ADR-0013): `persephone/profile` under the XDG data
/// directory.
pub(crate) fn default_profile_dir() -> Option<PathBuf> {
    data_home(env::var_os("XDG_DATA_HOME"), env::var_os("HOME"))
        .map(|base| base.join("persephone").join("profile"))
}

/// `$XDG_DATA_HOME`, else `$HOME/.local/share`. The XDG Base Directory rules
/// ignore a relative `XDG_DATA_HOME`; a missing or relative `HOME` gives
/// `None`.
fn data_home(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    super::unix::absolute(xdg_data_home)
        .or_else(|| super::unix::absolute(home).map(|home| home.join(".local").join("share")))
}

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

/// Starts a new core for a profile switch. Linux starts no UI yet, so no
/// switch is asked for; the start itself works as on macOS.
pub(crate) fn start_detached(command: &mut std::process::Command) -> std::io::Result<()> {
    command.spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        data_home(xdg.map(OsString::from), home.map(OsString::from))
    }

    /// GIVEN an absolute XDG_DATA_HOME THEN it is used; GIVEN a missing,
    /// empty or relative one THEN `$HOME/.local/share` is used; GIVEN no
    /// usable HOME either THEN there is no default.
    #[test]
    fn given_xdg_and_home_values_then_the_data_directory_follows_the_xdg_rules() {
        assert_eq!(
            data(Some("/xdg"), Some("/home/a")),
            Some(PathBuf::from("/xdg"))
        );
        for xdg in [None, Some(""), Some("relative/xdg")] {
            assert_eq!(
                data(xdg, Some("/home/a")),
                Some(PathBuf::from("/home/a/.local/share")),
                "{xdg:?}"
            );
        }
        for home in [None, Some(""), Some("home/a")] {
            assert_eq!(data(None, home), None, "{home:?}");
        }
    }
}
