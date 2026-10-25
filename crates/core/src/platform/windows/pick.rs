//! The Windows file dialog (`GetOpenFileNameW`) and the Windows read of a
//! picked file, under the rules in [`crate::pick`].

use crate::media::MAX_ENCODED_BYTES;
use crate::{media::Encoded, pick::ImagePicker};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::path::{Path, PathBuf};
use std::{
    ffi::OsString,
    fs::OpenOptions,
    os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::GENERIC_READ,
    Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ,
    },
    UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, OFN_DONTADDTORECENT, OFN_EXPLORER,
        OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_NODEREFERENCELINKS,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    },
};

/// Room for the longest path the dialog can return, in UTF-16 units.
const MAX_PICKED_PATH_UNITS: usize = 32_768;

pub fn read(path: &Path, budgets: &ResourceBudgets) -> Result<Encoded, ErrorCode> {
    let mut file = OpenOptions::new()
        .access_mode(GENERIC_READ)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
            _ => ErrorCode::Rejected,
        })?;
    // From the open handle (GetFileInformationByHandle), so it describes
    // what is read, not what the path names now.
    let metadata = file.metadata().map_err(|_| ErrorCode::Rejected)?;
    if metadata.file_attributes() & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || !metadata.is_file()
    {
        return Err(ErrorCode::Rejected);
    }
    let size = metadata.len();
    if size == 0 || size > MAX_ENCODED_BYTES {
        return Err(ErrorCode::Rejected);
    }
    let mut encoded = Encoded::reserve(budgets, size)?;
    encoded.fill_exactly(&mut file)?;
    Ok(encoded)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub struct NativeDialog;

impl ImagePicker for NativeDialog {
    fn pick(&self) -> Result<Option<PathBuf>, ErrorCode> {
        // COM for the dialog's shell views.
        let _apartment = super::Apartment::enter();
        let filter: Vec<u16> = "PNG or JPEG image\0*.png;*.jpg;*.jpeg\0\0"
            .encode_utf16()
            .collect();
        let title = wide("Choose an image to send");
        let mut file = vec![0u16; MAX_PICKED_PATH_UNITS];
        // SAFETY: OPENFILENAMEW is a plain C struct; all-zero is its
        // documented empty state.
        let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        dialog.hwndOwner = null_mut();
        dialog.lpstrFilter = filter.as_ptr();
        dialog.nFilterIndex = 1;
        dialog.lpstrFile = file.as_mut_ptr();
        dialog.nMaxFile = file.len() as u32;
        dialog.lpstrTitle = title.as_ptr();
        dialog.Flags = OFN_EXPLORER
            | OFN_FILEMUSTEXIST
            | OFN_PATHMUSTEXIST
            | OFN_NOCHANGEDIR
            | OFN_DONTADDTORECENT
            | OFN_NODEREFERENCELINKS
            | OFN_HIDEREADONLY;
        // SAFETY: every pointer refers to a buffer that outlives the call,
        // and `nMaxFile` is the file buffer's length.
        if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
            // SAFETY: no arguments.
            return match unsafe { CommDlgExtendedError() } {
                0 => Ok(None),
                _ => Err(ErrorCode::Unavailable),
            };
        }
        let length = file
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(file.len());
        Ok(Some(PathBuf::from(OsString::from_wide(&file[..length]))))
    }
}

#[cfg(test)]
mod tests {
    use crate::pick::read_image_file;
    use persephone_core_protocol::ErrorCode;
    use persephone_media_protocol::ResourceBudgets;
    use std::io::Write;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// GIVEN another process holds the file open for writing WHEN it is read
    /// THEN the read-only, read-shared open fails, so nothing can change the
    /// file during the read.
    #[test]
    fn given_a_file_open_for_writing_then_it_is_refused() {
        let dir = temp();
        let path = dir.path().join("busy.png");
        let mut writer = std::fs::File::create(&path).unwrap();
        writer.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        assert_eq!(
            read_image_file(&path, &ResourceBudgets::default()).err(),
            Some(ErrorCode::Rejected)
        );
        drop(writer);
        assert!(read_image_file(&path, &ResourceBudgets::default()).is_ok());
    }

    /// GIVEN a directory junction (which needs no administrator rights) to a
    /// folder holding an image WHEN the junction is read THEN it is refused
    /// as a reparse point. A file symbolic link needs a privilege the test
    /// may lack; it is checked only when it can be created.
    #[test]
    fn given_reparse_points_then_they_are_refused() {
        let dir = temp();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let image = target.join("photo.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let junction = dir.path().join("junction");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J failed");
        let budgets = ResourceBudgets::default();
        assert_eq!(
            read_image_file(&junction, &budgets).err(),
            Some(ErrorCode::Rejected)
        );
        let link = dir.path().join("link.png");
        if std::os::windows::fs::symlink_file(&image, &link).is_ok() {
            assert_eq!(
                read_image_file(&link, &budgets).err(),
                Some(ErrorCode::Rejected),
                "a file symbolic link is a reparse point"
            );
        } else {
            eprintln!("file symbolic link not created (no privilege); that case was not run");
        }
    }
}
