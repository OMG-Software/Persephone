//! Choosing an image file to send (ADR-0009). The core opens the file dialog
//! itself, so the page never names a path, and reads the chosen file once,
//! read-only:
//!
//! * The dialog ([`ImagePicker`]) shows PNG and JPEG files, does not follow
//!   shortcuts, does not change the working directory and does not add the
//!   file to the recent list. Tests and the development harness supply a
//!   file through the same trait without a dialog.
//! * [`read_image_file`] opens the file for reading, shared for reading only
//!   (no other process can open it for writing meanwhile), without following
//!   a reparse point. It refuses a reparse point (symbolic link or junction),
//!   a directory, anything that is not a plain file, an empty file and one
//!   larger than [`MAX_ENCODED_BYTES`]. The buffer is reserved before it is
//!   allocated, and the read fails if the length differs from the size
//!   checked first. The file is never written.

use crate::media::{Encoded, MAX_ENCODED_BYTES};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::path::{Path, PathBuf};

/// Asks the user for an image file in the core's own window.
pub trait ImagePicker: Send + Sync {
    /// The chosen file, or `None` when the user cancelled.
    fn pick(&self) -> Result<Option<PathBuf>, ErrorCode>;
}

/// No file dialog on this platform: `unavailable`.
pub struct NoPicker;

impl ImagePicker for NoPicker {
    fn pick(&self) -> Result<Option<PathBuf>, ErrorCode> {
        Err(ErrorCode::Unavailable)
    }
}

/// The platform's file dialog.
pub fn platform() -> Box<dyn ImagePicker> {
    #[cfg(windows)]
    return Box::new(windows::NativeDialog);
    #[cfg(not(windows))]
    return Box::new(NoPicker);
}

/// Reads a picked file under the rules in the module documentation. A
/// missing file is `not_found`; every other refusal is `rejected`, except a
/// full budget (`limit_exceeded`).
pub fn read_image_file(path: &Path, budgets: &ResourceBudgets) -> Result<Encoded, ErrorCode> {
    #[cfg(windows)]
    return windows::read(path, budgets);
    #[cfg(not(windows))]
    {
        let _ = (path, budgets);
        Err(ErrorCode::Unavailable)
    }
}

#[cfg(windows)]
mod windows {
    use super::{
        Encoded, ErrorCode, ImagePicker, MAX_ENCODED_BYTES, Path, PathBuf, ResourceBudgets,
    };
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
        System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
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
        if metadata.file_attributes() & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            != 0
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

    /// COM for the dialog's shell views, on this thread only.
    struct Apartment(bool);

    impl Apartment {
        fn enter() -> Self {
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

    pub struct NativeDialog;

    impl ImagePicker for NativeDialog {
        fn pick(&self) -> Result<Option<PathBuf>, ErrorCode> {
            let _apartment = Apartment::enter();
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
}

#[cfg(feature = "dev-disposable-servers")]
pub mod dev {
    //! Development only: answers the file dialog from a file, for automated
    //! runs against the disposable homeservers.

    use super::{ErrorCode, ImagePicker, PathBuf};
    use crate::credentials::dev::local_file;
    use std::io::Read;

    const MAX_ANSWER_BYTES: u64 = 4 * 1024;

    /// Reads `answer` each time it is asked: empty (or missing) cancels;
    /// otherwise it names the image to pick. Both the answer file and the
    /// image must be inside a `.local` directory (ignored by git).
    pub struct AnswerFile {
        answer: PathBuf,
    }

    impl AnswerFile {
        pub fn new(answer: PathBuf) -> Self {
            Self { answer }
        }
    }

    impl ImagePicker for AnswerFile {
        fn pick(&self) -> Result<Option<PathBuf>, ErrorCode> {
            let answer = local_file(&self.answer).map_err(|_| ErrorCode::Unavailable)?;
            let mut text = String::new();
            match std::fs::File::open(answer) {
                Ok(file) => {
                    file.take(MAX_ANSWER_BYTES)
                        .read_to_string(&mut text)
                        .map_err(|_| ErrorCode::Unavailable)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(ErrorCode::Unavailable),
            }
            let chosen = text.trim();
            if chosen.is_empty() {
                return Ok(None);
            }
            local_file(chosen.as_ref())
                .map(Some)
                .map_err(|_| ErrorCode::Unavailable)
        }
    }
}

#[cfg(all(test, feature = "dev-disposable-servers"))]
mod dev_tests {
    use super::{ImagePicker, dev::AnswerFile};

    /// GIVEN the harness answer file WHEN the dialog is asked THEN a missing
    /// or empty answer cancels, a path inside `.local` is picked, and a path
    /// outside it (or an answer file outside it) is refused.
    #[test]
    fn given_an_answer_file_then_only_local_images_are_picked() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join(".local");
        std::fs::create_dir(&local).unwrap();
        let answer = local.join("pick.txt");
        let picker = AnswerFile::new(answer.clone());
        assert_eq!(picker.pick(), Ok(None), "no answer yet");
        std::fs::write(&answer, "  \n").unwrap();
        assert_eq!(picker.pick(), Ok(None), "an empty answer cancels");
        let image = local.join("photo.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(&answer, image.to_str().unwrap()).unwrap();
        let picked = picker.pick().unwrap().unwrap();
        assert_eq!(
            std::fs::canonicalize(picked).unwrap(),
            std::fs::canonicalize(&image).unwrap()
        );
        let outside = root.path().join("outside.png");
        std::fs::write(&outside, b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(&answer, outside.to_str().unwrap()).unwrap();
        assert!(picker.pick().is_err());
        let stray = AnswerFile::new(root.path().join("pick.txt"));
        assert!(stray.pick().is_err());
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use persephone_media_protocol::{BudgetClass, MIB};
    use std::io::Write;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// GIVEN a small file WHEN read THEN its bytes come back exactly, the
    /// reservation is its size, and the file is unchanged afterwards.
    #[test]
    fn given_a_plain_file_then_it_is_read_once_and_left_unchanged() {
        let dir = temp();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"\xff\xd8\xff not really a jpeg").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let budgets = ResourceBudgets::default();
        let encoded = read_image_file(&path, &budgets).unwrap();
        assert_eq!(encoded.bytes(), b"\xff\xd8\xff not really a jpeg");
        assert_eq!(budgets.class_used(BudgetClass::EncodedInFlight), 21);
        drop(encoded);
        assert_eq!(budgets.class_used(BudgetClass::EncodedInFlight), 0);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xff\xd8\xff not really a jpeg"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }

    /// GIVEN files over the cap, empty, missing, a directory WHEN read THEN
    /// each is refused, and nothing stays reserved.
    #[test]
    fn given_oversized_empty_missing_or_directory_paths_then_they_are_refused() {
        let dir = temp();
        let budgets = ResourceBudgets::default();
        let large = dir.path().join("large.png");
        // Sparse: one byte over the cap without writing 20 MiB.
        std::fs::File::create(&large)
            .unwrap()
            .set_len(MAX_ENCODED_BYTES + 1)
            .unwrap();
        let exact = dir.path().join("exact.png");
        std::fs::File::create(&exact)
            .unwrap()
            .set_len(20 * MIB)
            .unwrap();
        let empty = dir.path().join("empty.png");
        std::fs::File::create(&empty).unwrap();
        for (path, expected) in [
            (large, ErrorCode::Rejected),
            (empty, ErrorCode::Rejected),
            (dir.path().join("missing.png"), ErrorCode::NotFound),
            (dir.path().to_path_buf(), ErrorCode::Rejected),
        ] {
            assert_eq!(
                read_image_file(&path, &budgets).err(),
                Some(expected),
                "{}",
                path.display()
            );
            assert_eq!(budgets.class_used(BudgetClass::EncodedInFlight), 0);
        }
        assert_eq!(
            read_image_file(&exact, &budgets).unwrap().bytes().len() as u64,
            MAX_ENCODED_BYTES,
            "exactly the cap is accepted"
        );
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
