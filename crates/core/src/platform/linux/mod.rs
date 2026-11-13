//! The Linux platform layer (ADR-0010): a Secret Service item for the
//! wrapping secret ([`vault`]), GTK3 prompts and file choosers on a
//! `core-gtk` thread ([`credentials`], [`pick`]), links opened through the
//! OpenURI portal ([`portal`]), the UI in its own process group and a
//! memory-capped systemd user scope ([`ui`]), the shared Unix read rules for
//! a picked file, and the media worker under its seccomp allowlist.
//!
//! Every service the layer needs fails closed when it is missing: no Secret
//! Service keeps the profile locked, no display refuses the prompts and
//! choosers, no portal opens no link. The one exception is the one ADR-0010
//! names: without a systemd user manager, the UI starts with a visible
//! warning that its memory is not capped.

pub mod bus;
pub mod credentials;
pub mod gtk_thread;
pub mod pick;
pub mod portal;
pub mod ui;
pub mod vault;

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
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

/// No idle time is read on Linux yet, so the idle lock is unavailable
/// (ADR-0025). The owner has not yet chosen the sources (for example the
/// session's idle hint or a desktop-specific idle monitor), so none is
/// guessed here.
pub(crate) fn idle_time() -> Option<std::time::Duration> {
    None
}

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

/// The store key's outer layer: the shared XChaCha20-Poly1305 wrap under a
/// secret kept in the user's Secret Service.
pub(crate) fn key_protector() -> Arc<dyn KeyProtector> {
    Arc::new(super::unix::Wrapped(vault::SecretService::session_bus()))
}

/// GTK3 prompts on the `core-gtk` thread.
pub(crate) fn credential_source() -> Box<dyn CredentialSource> {
    Box::new(credentials::NativePrompt)
}

/// `FileChooserNative` on the `core-gtk` thread.
pub(crate) fn image_picker() -> Box<dyn ImagePicker> {
    Box::new(pick::NativeChooser)
}

/// The shared Unix read rules with `O_NOFOLLOW` and `fstat`.
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
/// launcher and the worker's seccomp allowlist, confirmed through
/// `/proc/<pid>/status` before input is sent (ADR-0010).
pub(crate) fn image_worker() -> Arc<dyn ImageWorker> {
    Arc::new(media::contained::ContainedWorker::next_to_core())
}

/// The UI in its own process group and a systemd user scope with
/// `MemoryMax`.
pub(crate) fn ui_host() -> Box<dyn UiHost> {
    Box::new(ui::ScopeHost::default())
}

/// Starts a new core for a profile switch. It is not in the UI's process
/// group or scope, so ending the old UI does not end it.
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
