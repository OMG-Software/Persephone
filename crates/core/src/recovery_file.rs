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
//!   than followed or replaced, and so is anything that is not a plain file.
//! * The key is written to a new temporary file beside the chosen one,
//!   flushed to disk, and only then renamed over it (the dialog already asked
//!   before replacing a file). A failed write leaves an existing file as it
//!   was, and removes the temporary one.
//! * On Unix the file is readable by its owner only: the temporary file is
//!   created so. On Windows it takes its folder's access rules; under the
//!   user's own folders those are the user's.
//! * The written bytes are zeroed afterwards. The file system's own copies,
//!   and the file itself, are the user's to keep safe; the window says so.

use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

/// Why the key was not saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The path is a link, a directory or another special file.
    NotAFile,
    /// The file could not be created, written, flushed or put in place.
    Failed,
}

/// Whether `path` may be replaced: absent, or a plain file that is not a
/// link or other reparse point.
fn replaceable(path: &Path) -> Result<(), SaveError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(SaveError::Failed),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SaveError::NotAFile);
        }
    }
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SaveError::NotAFile);
    }
    Ok(())
}

/// A new temporary file beside `path`, created only if no file has its name
/// (owner-only on Unix), with that name.
fn create_beside(path: &Path) -> Result<(File, PathBuf), SaveError> {
    let parent = path.parent().ok_or(SaveError::Failed)?;
    let name = path.file_name().ok_or(SaveError::Failed)?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| SaveError::Failed)?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".{suffix}.tmp"));
    let temporary = parent.join(temporary_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&temporary).map_err(|_| SaveError::Failed)?;
    Ok((file, temporary))
}

/// Writes `key` and a line break to `path` under the rules above.
pub fn write(path: &Path, key: &str) -> Result<(), SaveError> {
    replaceable(path)?;
    let (mut file, temporary) = create_beside(path)?;
    let mut contents = Zeroizing::new(Vec::with_capacity(key.len() + 1));
    contents.extend_from_slice(key.as_bytes());
    contents.push(b'\n');
    let written = file
        .write_all(&contents)
        .and_then(|()| file.sync_all())
        .map_err(|_| SaveError::Failed);
    drop(file);
    // Checked again just before the rename, which replaces the name itself:
    // even a link put there meanwhile would be replaced, never followed.
    let placed = written
        .and_then(|()| replaceable(path))
        .and_then(|()| std::fs::rename(&temporary, path).map_err(|_| SaveError::Failed));
    if placed.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return placed;
    }
    // The rename is durable once the folder is flushed (Unix; Windows
    // flushes its metadata itself).
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(folder) = File::open(parent)
    {
        let _ = folder.sync_all();
    }
    Ok(())
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
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(write(&folder, "KEY"), Err(SaveError::NotAFile));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no file left"
        );
    }

    /// GIVEN saves that succeed and fail WHEN they end THEN no temporary file
    /// stays beside the chosen one, and a failed save leaves the old file.
    #[test]
    fn given_saves_then_no_temporary_file_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.txt");
        write(&path, "FIRST").unwrap();
        write(&path, "SECOND").unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["key.txt"]);
        assert_eq!(std::fs::read(&path).unwrap(), b"SECOND\n");
        // A folder that does not exist: nothing can be created, nothing changes.
        let missing = dir.path().join("missing").join("key.txt");
        assert_eq!(write(&missing, "THIRD"), Err(SaveError::Failed));
        assert_eq!(std::fs::read(&path).unwrap(), b"SECOND\n");
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
