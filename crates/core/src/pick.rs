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
//!   larger than [`MAX_ENCODED_BYTES`](crate::media::MAX_ENCODED_BYTES). The buffer is reserved before it is
//!   allocated, and the read fails if the length differs from the size
//!   checked first. The file is never written.

use crate::media::Encoded;
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::path::{Path, PathBuf};

/// Asks the user for an image file in the core's own window.
pub trait ImagePicker: Send + Sync {
    /// The chosen file, or `None` when the user cancelled.
    fn pick(&self) -> Result<Option<PathBuf>, ErrorCode>;

    /// Any file to send (ADR-0029), or `None` when the user cancelled.
    /// Unavailable unless the platform provides it.
    fn pick_file(&self) -> Result<Option<PathBuf>, ErrorCode> {
        Err(ErrorCode::Unavailable)
    }

    /// Where to save a received file, offering `suggested` as its name;
    /// `None` when the user cancelled. Unavailable unless the platform
    /// provides it.
    fn save_file(&self, _suggested: &str) -> Result<Option<PathBuf>, ErrorCode> {
        Err(ErrorCode::Unavailable)
    }
}

/// No file dialog on this platform: `unavailable`.
pub struct NoPicker;

impl ImagePicker for NoPicker {
    fn pick(&self) -> Result<Option<PathBuf>, ErrorCode> {
        Err(ErrorCode::Unavailable)
    }
}

/// The platform's file dialog ([`crate::platform`]); [`NoPicker`] where none
/// is implemented.
pub fn platform() -> Box<dyn ImagePicker> {
    crate::platform::os::image_picker()
}

/// Reads a picked file under the rules in the module documentation. A
/// missing file is `not_found`; every other refusal is `rejected`, except a
/// full budget (`limit_exceeded`).
/// Platforms without an implemented read return `unavailable`.
pub fn read_image_file(path: &Path, budgets: &ResourceBudgets) -> Result<Encoded, ErrorCode> {
    crate::platform::os::read_image_file(path, budgets)
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

/// The read rules every platform's read must meet. They run where a read is
/// implemented; the Windows-only cases (share mode, reparse points) are in
/// `platform::windows::pick`.
#[cfg(all(test, any(windows, target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::media::MAX_ENCODED_BYTES;
    use persephone_media_protocol::{BudgetClass, MIB};

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
}
