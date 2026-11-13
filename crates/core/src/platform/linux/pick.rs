//! The Linux file choosers (ADR-0009, ADR-0010, ADR-0029): GTK's
//! `FileChooserNative` on the `core-gtk` thread, which is the desktop's
//! file chooser portal where one is used and GTK's own dialog otherwise.
//!
//! * [`pick`](ImagePicker::pick) offers PNG and JPEG files only;
//!   [`pick_file`](ImagePicker::pick_file) offers any file.
//! * [`save_file`](ImagePicker::save_file) and the recovery key's Save
//!   chooser suggest a name and ask before replacing a file.
//! * Only local files are offered, one at a time; chosen files are not added
//!   to the recent-files list (`gtk_thread::init_gtk`).
//!
//! The chooser returns the path the user chose. The core then reads it with
//! the shared Unix rules (`unix::read_image_file`: `O_NOFOLLOW`, `fstat`,
//! size limits) and saves with an owner-only file (`unix::create_private_file`).

use super::credentials::without_nul;
use super::gtk_thread::{gtk_thread, settle};
use crate::pick::ImagePicker;
use gtk::prelude::*;
use persephone_core_protocol::ErrorCode;
use std::path::PathBuf;

/// The image types [`ImagePicker::pick`] offers.
pub const IMAGE_MIME_TYPES: [&str; 2] = ["image/png", "image/jpeg"];
/// File name patterns for the same types, for a chooser without MIME data.
pub const IMAGE_PATTERNS: [&str; 6] = ["*.png", "*.PNG", "*.jpg", "*.JPG", "*.jpeg", "*.JPEG"];

/// What a chooser asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Image,
    AnyFile,
}

pub struct NativeChooser;

impl ImagePicker for NativeChooser {
    fn pick(&self) -> Result<Option<PathBuf>, ErrorCode> {
        open_chooser(Choice::Image)
    }

    fn pick_file(&self) -> Result<Option<PathBuf>, ErrorCode> {
        open_chooser(Choice::AnyFile)
    }

    fn save_file(&self, suggested: &str) -> Result<Option<PathBuf>, ErrorCode> {
        save_chooser("Save the file", suggested)
    }
}

/// The result of a closed chooser: Accept with an absolute local path picks
/// it, Cancel or closing picks nothing, and anything else (no local path,
/// or a chooser that did not run) is `unavailable`.
pub fn chosen(
    response: gtk::ResponseType,
    path: Option<PathBuf>,
) -> Result<Option<PathBuf>, ErrorCode> {
    match (response, path) {
        (gtk::ResponseType::Accept, Some(path)) if path.is_absolute() => Ok(Some(path)),
        (gtk::ResponseType::Cancel | gtk::ResponseType::DeleteEvent, _) => Ok(None),
        _ => Err(ErrorCode::Unavailable),
    }
}

fn open_chooser(choice: Choice) -> Result<Option<PathBuf>, ErrorCode> {
    gtk_thread()
        .run(move || {
            let title = match choice {
                Choice::Image => "Choose an image to send",
                Choice::AnyFile => "Choose a file to send to this room",
            };
            let chooser = gtk::FileChooserNative::new(
                Some(title),
                None::<&gtk::Window>,
                gtk::FileChooserAction::Open,
                Some("Open"),
                Some("Cancel"),
            );
            chooser.set_local_only(true);
            chooser.set_select_multiple(false);
            if choice == Choice::Image {
                let filter = gtk::FileFilter::new();
                filter.set_name(Some("PNG or JPEG image"));
                for mime in IMAGE_MIME_TYPES {
                    filter.add_mime_type(mime);
                }
                for pattern in IMAGE_PATTERNS {
                    filter.add_pattern(pattern);
                }
                chooser.add_filter(filter);
            }
            run(&chooser)
        })
        .unwrap_or(Err(ErrorCode::Unavailable))
}

/// Asks where to save a file, suggesting `suggested` as its name.
/// `Ok(None)` when the user cancelled; `unavailable` when no chooser can be
/// shown.
pub fn save_chooser(title: &str, suggested: &str) -> Result<Option<PathBuf>, ErrorCode> {
    let title = without_nul(title);
    let suggested = without_nul(suggested);
    gtk_thread()
        .run(move || {
            let chooser = gtk::FileChooserNative::new(
                Some(&title),
                None::<&gtk::Window>,
                gtk::FileChooserAction::Save,
                Some("Save"),
                Some("Cancel"),
            );
            chooser.set_local_only(true);
            chooser.set_do_overwrite_confirmation(true);
            chooser.set_create_folders(true);
            chooser.set_current_name(&suggested);
            run(&chooser)
        })
        .unwrap_or(Err(ErrorCode::Unavailable))
}

fn run(chooser: &gtk::FileChooserNative) -> Result<Option<PathBuf>, ErrorCode> {
    chooser.set_modal(true);
    let response = chooser.run();
    let path = chooser.filename();
    chooser.destroy();
    settle();
    chosen(response, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN how the chooser closed THEN Accept with an absolute path picks
    /// it, Cancel or closing picks nothing, and Accept without a local or
    /// absolute path, or any other answer, is unavailable.
    #[test]
    fn given_chooser_results_then_only_accept_with_a_local_path_picks() {
        let path = PathBuf::from("/home/a/photo.png");
        assert_eq!(
            chosen(gtk::ResponseType::Accept, Some(path.clone())),
            Ok(Some(path.clone()))
        );
        for closed in [gtk::ResponseType::Cancel, gtk::ResponseType::DeleteEvent] {
            assert_eq!(chosen(closed, Some(path.clone())), Ok(None));
        }
        assert_eq!(
            chosen(gtk::ResponseType::Accept, None),
            Err(ErrorCode::Unavailable)
        );
        assert_eq!(
            chosen(gtk::ResponseType::Accept, Some(PathBuf::from("photo.png"))),
            Err(ErrorCode::Unavailable)
        );
        assert_eq!(
            chosen(gtk::ResponseType::None, Some(path)),
            Err(ErrorCode::Unavailable)
        );
    }

    /// GIVEN the image chooser THEN it offers PNG and JPEG only, by type and
    /// by name in either case.
    #[test]
    fn given_the_image_chooser_then_it_offers_png_and_jpeg_only() {
        assert_eq!(IMAGE_MIME_TYPES, ["image/png", "image/jpeg"]);
        for pattern in IMAGE_PATTERNS {
            let extension = pattern.trim_start_matches("*.").to_ascii_lowercase();
            assert!(["png", "jpg", "jpeg"].contains(&extension.as_str()));
        }
    }
}
