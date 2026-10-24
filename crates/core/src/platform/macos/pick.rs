//! The macOS file dialog: an `NSOpenPanel` on the main thread, for one PNG
//! or JPEG file. It does not resolve aliases, so it returns the path the user
//! chose; the read (`unix::read_image_file`) then refuses a symbolic link,
//! and an alias file is not an image. The panel does not create folders or
//! open packages.

use super::{credentials::bring_to_front, main_loop::main_thread};
use crate::pick::ImagePicker;
use objc2::{MainThreadMarker, rc::autoreleasepool};
use objc2_app_kit::NSOpenPanel;
use objc2_foundation::{NSArray, NSString};
use persephone_core_protocol::ErrorCode;
use std::path::PathBuf;

/// The extensions the panel offers.
pub const IMAGE_TYPES: [&str; 3] = ["png", "jpg", "jpeg"];
/// `NSModalResponseOK` and `NSModalResponseCancel`.
const PANEL_OK: isize = 1;
const PANEL_CANCEL: isize = 0;

pub struct NativePanel;

impl ImagePicker for NativePanel {
    fn pick(&self) -> Result<Option<PathBuf>, ErrorCode> {
        main_thread(|| {
            let mtm = MainThreadMarker::new().ok_or(ErrorCode::Unavailable)?;
            autoreleasepool(|_| show(mtm))
        })
        .unwrap_or(Err(ErrorCode::Unavailable))
    }
}

/// The result of a closed panel: OK with a path picks it, Cancel picks
/// nothing, and anything else (no path, or the panel failed) is
/// `unavailable`.
pub fn chosen(response: isize, path: Option<PathBuf>) -> Result<Option<PathBuf>, ErrorCode> {
    match (response, path) {
        (PANEL_OK, Some(path)) => Ok(Some(path)),
        (PANEL_CANCEL, _) => Ok(None),
        _ => Err(ErrorCode::Unavailable),
    }
}

fn show(mtm: MainThreadMarker) -> Result<Option<PathBuf>, ErrorCode> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    panel.setResolvesAliases(false);
    panel.setCanCreateDirectories(false);
    panel.setTreatsFilePackagesAsDirectories(false);
    panel.setAllowsOtherFileTypes(false);
    let types: Vec<_> = IMAGE_TYPES.iter().map(|t| NSString::from_str(t)).collect();
    // `allowedContentTypes` needs UTType, which is in no crate already used;
    // file name extensions still work on every supported macOS.
    #[allow(deprecated)]
    panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&types)));
    panel.setMessage(Some(&NSString::from_str(
        "Choose a PNG or JPEG image to send.",
    )));
    bring_to_front(mtm, &panel);
    let response = panel.runModal();
    let path = panel.URL().and_then(|url| url.to_file_path());
    chosen(response, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN how the panel closed THEN OK with a path picks it, Cancel picks
    /// nothing, and OK without a path or a failed panel is unavailable.
    #[test]
    fn given_panel_results_then_only_ok_with_a_path_picks() {
        let path = PathBuf::from("/Users/a/photo.png");
        assert_eq!(chosen(PANEL_OK, Some(path.clone())), Ok(Some(path.clone())));
        assert_eq!(chosen(PANEL_CANCEL, Some(path.clone())), Ok(None));
        assert_eq!(chosen(PANEL_OK, None), Err(ErrorCode::Unavailable));
        assert_eq!(chosen(-1000, Some(path)), Err(ErrorCode::Unavailable));
        assert_eq!(IMAGE_TYPES, ["png", "jpg", "jpeg"]);
    }

    /// GIVEN no main loop (this test process) THEN the dialog is
    /// unavailable at once.
    #[test]
    fn given_no_main_loop_then_the_dialog_is_unavailable() {
        assert_eq!(NativePanel.pick(), Err(ErrorCode::Unavailable));
    }
}
