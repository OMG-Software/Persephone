//! Saving a new recovery key to a file the user chose (ADR-0021).
//!
//! The key never reaches the UI process: the core shows it in its own window,
//! then asks where to save it in its own Save dialog
//! ([`CredentialSource::choose_recovery_key_file`](crate::credentials::CredentialSource::choose_recovery_key_file))
//! and writes the file here. The page never names a path.
//!
//! * The file holds the key and a line break, nothing else, so it can be
//!   pasted back into the recovery-key window as it is.
//! * A symbolic link, junction or other reparse point is refused rather
//!   than followed, and so is anything that is not a plain file. The file is
//!   opened first and checked through the open handle; only then is an
//!   existing one emptied (the dialog already asked before replacing it).
//! * On Unix the file is readable by its owner only: a new file is created
//!   so and an existing one is narrowed before it is written. On Windows it
//!   takes its folder's access rules; under the user's own folders those are
//!   the user's.
//! * The written bytes are zeroed afterwards. The file system's own copies,
//!   and the file itself, are the user's to keep safe; the window says so.

use std::{fs::File, io::Write, path::Path};
use zeroize::Zeroizing;

/// Why the key was not saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The path is a link, a directory or another special file.
    NotAFile,
    /// The file could not be opened, written or flushed.
    Failed,
}

/// Opens `path` for writing without following a link and without emptying
/// it, creating it if needed.
#[cfg(windows)]
fn open(path: &Path) -> Result<File, SaveError> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        // No other process may open it meanwhile.
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| SaveError::Failed)?;
    let metadata = file.metadata().map_err(|_| SaveError::Failed)?;
    if metadata.file_attributes() & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || !metadata.is_file()
    {
        return Err(SaveError::NotAFile);
    }
    Ok(file)
}

#[cfg(unix)]
fn open(path: &Path) -> Result<File, SaveError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            // O_NOFOLLOW on a link.
            Some(libc::ELOOP) => SaveError::NotAFile,
            _ => SaveError::Failed,
        })?;
    let metadata = file.metadata().map_err(|_| SaveError::Failed)?;
    if !metadata.is_file() {
        return Err(SaveError::NotAFile);
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| SaveError::Failed)?;
    Ok(file)
}

#[cfg(not(any(windows, unix)))]
fn open(_: &Path) -> Result<File, SaveError> {
    Err(SaveError::Failed)
}

/// Writes `key` and a line break to `path` under the rules above.
pub fn write(path: &Path, key: &str) -> Result<(), SaveError> {
    let mut file = open(path)?;
    let mut contents = Zeroizing::new(Vec::with_capacity(key.len() + 1));
    contents.extend_from_slice(key.as_bytes());
    contents.push(b'\n');
    file.set_len(0).map_err(|_| SaveError::Failed)?;
    file.write_all(&contents).map_err(|_| SaveError::Failed)?;
    file.sync_all().map_err(|_| SaveError::Failed)
}

#[cfg(all(test, any(windows, unix)))]
mod tests {
    use super::*;

    /// GIVEN a new path WHEN the key is saved THEN the file holds the key and
    /// a line break; on Unix only its owner may read it.
    #[test]
    fn given_a_new_path_then_the_key_is_written_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery-key.txt");
        write(&path, "EsTs abcd efgh").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"EsTs abcd efgh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// GIVEN an existing, longer file WHEN the key is saved THEN it is
    /// replaced whole, and on Unix narrowed to its owner.
    #[test]
    fn given_an_existing_file_then_it_is_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.txt");
        std::fs::write(&path, "a much longer old recovery key and more").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write(&path, "NEW KEY").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"NEW KEY\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// GIVEN a directory where the file should be WHEN the key is saved THEN
    /// it is refused and nothing is written.
    #[test]
    fn given_a_directory_then_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write(dir.path(), "KEY").is_err());
    }

    /// GIVEN a symbolic link to another file WHEN the key is saved through it
    /// THEN it is refused and the target is unchanged.
    #[cfg(unix)]
    #[test]
    fn given_a_symbolic_link_then_it_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(write(&link, "KEY"), Err(SaveError::NotAFile));
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    /// GIVEN a directory junction (no administrator rights needed) WHEN the
    /// key is saved to it THEN it is refused as a reparse point.
    #[cfg(windows)]
    #[test]
    fn given_a_junction_then_it_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let junction = dir.path().join("junction");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success());
        assert!(write(&junction, "KEY").is_err());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
}
