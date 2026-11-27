//! The app menu entry on Linux (ADR-0072): the AppImage checks and the file
//! work for [`crate::app_menu`].
//!
//! The entry goes to `$XDG_DATA_HOME/applications/Persephone.desktop` (else
//! `~/.local/share`), and its icons to `icons/hicolor/<size>/apps` there.
//! The icons are the bundle's PNG files, compiled in, so nothing is read
//! from the image or the disk to write them.
//!
//! Every file is written to a new file in the same folder (`O_EXCL`), synced
//! and then put in place in one step: a rename over Persephone's own older
//! file, or a hard link where no file was, which fails when a file appeared
//! meanwhile. A reader never sees part of a file, and a link at the target
//! is never followed. An existing entry is read with `O_NOFOLLOW`, at most
//! [`MAX_ENTRY_BYTES`]. The folders above the files are the user's own and
//! may be links (for example from a dotfile manager), as the XDG rules
//! allow. No other program is started.

use crate::app_menu::{
    self, AppMenu, AppMenuError, DESKTOP_FILE, Found, ICON_NAME, MAX_ENTRY_BYTES, Place,
};
use persephone_contracts::AppMenuState;
use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// The icons, by hicolor size folder. The 256 pixel icon is the bundle's
/// `128x128@2x.png`, as in the AppImage's own `256x256` folder.
const ICONS: [(&str, &[u8]); 3] = [
    (
        "32x32",
        include_bytes!("../../../../../src-tauri/icons/32x32.png"),
    ),
    (
        "128x128",
        include_bytes!("../../../../../src-tauri/icons/128x128.png"),
    ),
    (
        "256x256",
        include_bytes!("../../../../../src-tauri/icons/128x128@2x.png"),
    ),
];

/// Most `$XDG_DATA_DIRS` entries looked at.
const MAX_DATA_DIRS: usize = 64;

/// The XDG app menu of the user who runs the core.
pub struct XdgAppMenu;

impl AppMenu for XdgAppMenu {
    fn state(&self) -> AppMenuState {
        Places::from_env().map_or(AppMenuState::Unavailable, |places| places.state())
    }

    fn place(&self) -> Option<Place> {
        Places::from_env().map(|places| Place {
            appimage: places.appimage.clone(),
            data_home: places.data_home.display().to_string(),
        })
    }

    fn add(&self) -> Result<AppMenuState, AppMenuError> {
        let places = Places::from_env().ok_or(AppMenuError::Unavailable)?;
        places.add().map_err(|_| AppMenuError::Failed)
    }

    fn remove(&self) -> Result<AppMenuState, AppMenuError> {
        let places = Places::from_env().ok_or(AppMenuError::Unavailable)?;
        places.remove().map_err(|_| AppMenuError::Failed)
    }
}

/// The checked AppImage and the folders of one look at the menu.
#[derive(Debug)]
struct Places {
    appimage: String,
    data_home: PathBuf,
    data_dirs: Vec<PathBuf>,
}

impl Places {
    /// From this process's environment; `None` unless the core runs from a
    /// checked AppImage and the user has a data folder.
    fn from_env() -> Option<Self> {
        let exe = env::current_exe().ok()?;
        let (appimage, appdir) =
            checked_appimage(env::var_os("APPIMAGE"), env::var_os("APPDIR"), &exe)?;
        let data_home = super::data_home(env::var_os("XDG_DATA_HOME"), env::var_os("HOME"))?;
        let data_dirs = data_dirs(env::var_os("XDG_DATA_DIRS"), &data_home, &appdir);
        Some(Self {
            appimage,
            data_home,
            data_dirs,
        })
    }

    fn applications(&self) -> PathBuf {
        self.data_home.join("applications")
    }

    fn hicolor(&self) -> PathBuf {
        self.data_home.join("icons").join("hicolor")
    }

    fn state(&self) -> AppMenuState {
        let user = read_entry(&self.applications().join(DESKTOP_FILE));
        let elsewhere = self.data_dirs.iter().any(|dir| {
            // Anything there, or a folder that cannot be read, counts.
            fs::symlink_metadata(dir.join("applications").join(DESKTOP_FILE))
                .map_or_else(|error| error.kind() != io::ErrorKind::NotFound, |_| true)
        });
        app_menu::state(&user, elsewhere, &self.appimage)
    }

    /// Writes the icons and then the entry, when the state is `absent` or
    /// `outdated`; returns the new state.
    fn add(&self) -> io::Result<AppMenuState> {
        let replace = match self.state() {
            AppMenuState::Absent => false,
            AppMenuState::Outdated => true,
            other => return Ok(other),
        };
        let entry = app_menu::entry(&self.appimage)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let hicolor = self.hicolor();
        let icon = format!("{ICON_NAME}.png");
        for (size, bytes) in ICONS {
            let dir = hicolor.join(size).join("apps");
            make_dirs(&dir)?;
            put_file(&dir, &icon, bytes, true)?;
        }
        // GTK and Qt skip an `icon-theme.cache` that is older than its
        // theme folder; one there would not list the new icons.
        let _ = File::open(&hicolor).and_then(|dir| dir.set_modified(SystemTime::now()));
        let applications = self.applications();
        make_dirs(&applications)?;
        match put_file(&applications, DESKTOP_FILE, entry.as_bytes(), replace) {
            // A file appeared after the check: it is not replaced.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            result => result?,
        }
        Ok(self.state())
    }

    /// Removes the entry and the icons when the entry is Persephone's own;
    /// returns the new state.
    fn remove(&self) -> io::Result<AppMenuState> {
        let applications = self.applications();
        let entry = applications.join(DESKTOP_FILE);
        if let Found::Content(content) = read_entry(&entry)
            && app_menu::is_ours(&content)
        {
            remove_present(&entry)?;
            sync_dir(&applications);
            let icon = format!("{ICON_NAME}.png");
            for (size, _) in ICONS {
                remove_present(&self.hicolor().join(size).join("apps").join(&icon))?;
            }
        }
        Ok(self.state())
    }
}

/// The AppImage path and its mount folder when the core runs from it:
/// `APPIMAGE` is [`app_menu::nameable`] and its own canonical path, the file
/// there is an executable type 2 AppImage, and `exe` (the core's own
/// canonical path) is inside `APPDIR`, the image's mount folder, which is
/// canonical too.
fn checked_appimage(
    appimage: Option<OsString>,
    appdir: Option<OsString>,
    exe: &Path,
) -> Option<(String, PathBuf)> {
    let appimage = appimage?.into_string().ok()?;
    if !app_menu::nameable(&appimage) {
        return None;
    }
    let path = Path::new(&appimage);
    if fs::canonicalize(path).ok()? != path || !is_appimage_file(path) {
        return None;
    }
    let appdir = PathBuf::from(appdir?);
    if !appdir.is_absolute() || fs::canonicalize(&appdir).ok()? != appdir {
        return None;
    }
    (exe != appdir && exe.starts_with(&appdir)).then_some((appimage, appdir))
}

/// Whether `path`, opened without following a link, is a regular file with
/// an execute bit, the ELF magic and the AppImage type 2 magic `AI\x02` at
/// byte 8.
fn is_appimage_file(path: &Path) -> bool {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
    else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return false;
    }
    let mut head = [0_u8; 11];
    file.read_exact(&mut head).is_ok() && head[..4] == *b"\x7fELF" && head[8..] == *b"AI\x02"
}

/// The absolute folders of `$XDG_DATA_DIRS` (else the XDG default), at most
/// [`MAX_DATA_DIRS`], without the user's own data folder and without the
/// folders inside the image's mount folder `appdir`. The AppImage's GTK hook
/// puts `$APPDIR/usr/share` first in this process's list; the image's own
/// `Persephone.desktop` there is not in the desktop's menu.
fn data_dirs(value: Option<OsString>, data_home: &Path, appdir: &Path) -> Vec<PathBuf> {
    let listed: Vec<PathBuf> = value
        .filter(|value| !value.is_empty())
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();
    let dirs = if listed.iter().any(|dir| dir.is_absolute()) {
        listed
    } else {
        vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]
    };
    dirs.into_iter()
        .filter(|dir| dir.is_absolute() && dir != data_home && !dir.starts_with(appdir))
        .take(MAX_DATA_DIRS)
        .collect()
}

/// What is at `path`, read without following a link.
fn read_entry(path: &Path) -> Found {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Found::Missing,
        // A link (`ELOOP`), a folder that cannot be read, …
        Err(_) => return Found::Other,
    };
    let Ok(metadata) = file.metadata() else {
        return Found::Other;
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_ENTRY_BYTES {
        return Found::Other;
    }
    let mut content = Vec::new();
    // Bounded again while it is read: the file can grow meanwhile.
    match file.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut content) {
        Ok(read) if read as u64 <= MAX_ENTRY_BYTES => Found::Content(content),
        _ => Found::Other,
    }
}

fn make_dirs(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
}

/// Puts `bytes` at `dir/name` in one step: a new file next to it, synced,
/// then renamed over the target (`replace`) or linked to it, which fails
/// with `AlreadyExists` when the target exists. The new file is removed on
/// every path but a rename.
fn put_file(dir: &Path, name: &str, bytes: &[u8], replace: bool) -> io::Result<()> {
    let target = dir.join(name);
    let partial = dir.join(format!(".{name}.{}.partial", std::process::id()));
    let create = || {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&partial)
    };
    // A file of this name is left from an earlier core with the same
    // process ID that stopped while it wrote.
    let mut file = match create() {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(&partial)?;
            create()?
        }
        result => result?,
    };
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            if replace {
                fs::rename(&partial, &target)
            } else {
                fs::hard_link(&partial, &target)
            }
        });
    if result.is_err() || !replace {
        let _ = fs::remove_file(&partial);
    }
    result?;
    sync_dir(dir);
    Ok(())
}

/// Deletes `path` itself (a link is not followed); a missing file is fine.
fn remove_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Syncs a folder so a new or removed name survives a crash. A folder that
/// cannot be synced does not undo the change.
fn sync_dir(dir: &Path) {
    let _ = File::open(dir).and_then(|dir| dir.sync_all());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake type 2 AppImage: the ELF magic, the AppImage magic at byte 8,
    /// and an execute bit.
    fn fake_appimage(path: &Path) {
        let mut head = b"\x7fELF\x02\x01\x01\x00AI\x02".to_vec();
        head.resize(64, 0);
        fs::write(path, head).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A temporary layout: an AppImage, its mount folder with the core
    /// inside, the user's data folder and one system data folder.
    struct Layout {
        _root: tempfile::TempDir,
        appimage: PathBuf,
        appdir: PathBuf,
        exe: PathBuf,
        data_home: PathBuf,
        system: PathBuf,
    }

    impl Layout {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(root.path()).unwrap();
            let appimage = base.join("Persephone (1).AppImage");
            fake_appimage(&appimage);
            let appdir = base.join(".mount_Persep");
            fs::create_dir_all(appdir.join("usr/bin")).unwrap();
            let exe = appdir.join("usr/bin/persephone");
            fs::write(&exe, b"").unwrap();
            let data_home = base.join("home/.local/share");
            let system = base.join("usr/share");
            fs::create_dir_all(&data_home).unwrap();
            fs::create_dir_all(system.join("applications")).unwrap();
            Self {
                _root: root,
                appimage,
                appdir,
                exe,
                data_home,
                system,
            }
        }

        fn places(&self) -> Places {
            Places {
                appimage: self.appimage.to_str().unwrap().to_owned(),
                data_home: self.data_home.clone(),
                data_dirs: vec![self.system.clone()],
            }
        }

        fn check(&self, appimage: &Path) -> Option<String> {
            checked_appimage(
                Some(appimage.as_os_str().to_owned()),
                Some(self.appdir.as_os_str().to_owned()),
                &self.exe,
            )
            .map(|(appimage, _)| appimage)
        }

        fn entry(&self) -> PathBuf {
            self.data_home.join("applications").join(DESKTOP_FILE)
        }

        fn icon(&self, size: &str) -> PathBuf {
            self.data_home
                .join("icons/hicolor")
                .join(size)
                .join("apps")
                .join(format!("{ICON_NAME}.png"))
        }
    }

    /// GIVEN the core inside the mount folder of a canonical, executable
    /// type 2 AppImage WHEN checked THEN its path and the mount folder are
    /// used.
    #[test]
    fn given_a_mounted_appimage_when_checked_then_its_path_is_used() {
        let layout = Layout::new();
        assert_eq!(
            checked_appimage(
                Some(layout.appimage.clone().into()),
                Some(layout.appdir.clone().into()),
                &layout.exe,
            ),
            Some((
                layout.appimage.to_str().unwrap().to_owned(),
                layout.appdir.clone()
            ))
        );
    }

    /// GIVEN the image's own `Persephone.desktop` under `$APPDIR/usr/share`,
    /// which the AppImage's GTK hook puts first in `XDG_DATA_DIRS`, WHEN the
    /// state is read THEN that folder does not count and the state is
    /// `absent`.
    #[test]
    fn given_the_images_own_entry_when_read_then_it_does_not_count() {
        let layout = Layout::new();
        let inside = layout.appdir.join("usr/share");
        fs::create_dir_all(inside.join("applications")).unwrap();
        fs::write(
            inside.join("applications").join(DESKTOP_FILE),
            "[Desktop Entry]\nName=Persephone\nExec=AppRun\n",
        )
        .unwrap();
        let listed = env::join_paths([&inside, &layout.system]).unwrap();
        let places = Places {
            data_dirs: data_dirs(Some(listed), &layout.data_home, &layout.appdir),
            ..layout.places()
        };
        assert_eq!(places.data_dirs, [layout.system.clone()]);
        assert_eq!(places.state(), AppMenuState::Absent);
    }

    /// GIVEN no APPIMAGE, a relative, non-canonical or linked path, a file
    /// without the magic or the execute bit, a folder, no APPDIR, or a core
    /// outside APPDIR WHEN checked THEN there is no AppImage, so the page
    /// shows nothing and nothing is written.
    #[test]
    fn given_wrong_appimage_values_when_checked_then_they_are_refused() {
        let layout = Layout::new();
        let base = layout.appimage.parent().unwrap().to_path_buf();
        let exe = &layout.exe;
        let appdir = || Some(layout.appdir.as_os_str().to_owned());
        assert_eq!(checked_appimage(None, appdir(), exe), None);
        assert_eq!(
            checked_appimage(Some("Persephone (1).AppImage".into()), appdir(), exe),
            None
        );
        let dotted = base.join("home/../Persephone (1).AppImage");
        assert_eq!(layout.check(&dotted), None, "not canonical");
        let link = base.join("link.AppImage");
        symlink(&layout.appimage, &link).unwrap();
        assert_eq!(layout.check(&link), None, "a link");
        let plain = base.join("plain.AppImage");
        fs::write(&plain, vec![0_u8; 64]).unwrap();
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(layout.check(&plain), None, "no magic");
        let type1 = base.join("type1.AppImage");
        let mut head = b"\x7fELF\x02\x01\x01\x00AI\x01".to_vec();
        head.resize(64, 0);
        fs::write(&type1, head).unwrap();
        fs::set_permissions(&type1, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(layout.check(&type1), None, "type 1");
        let unexecutable = base.join("unexecutable.AppImage");
        fake_appimage(&unexecutable);
        fs::set_permissions(&unexecutable, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(layout.check(&unexecutable), None, "no execute bit");
        assert_eq!(layout.check(&layout.appdir), None, "a folder");
        let newline = base.join("x\nExec=sh.AppImage");
        fake_appimage(&newline);
        assert_eq!(layout.check(&newline), None, "a control character");
        assert_eq!(
            checked_appimage(Some(layout.appimage.clone().into()), None, exe),
            None,
            "no APPDIR"
        );
        let outside = base.join("persephone");
        assert_eq!(
            checked_appimage(Some(layout.appimage.clone().into()), appdir(), &outside),
            None,
            "the core is not in the image"
        );
        let sibling = PathBuf::from(format!("{}x/usr/bin/persephone", layout.appdir.display()));
        assert_eq!(
            checked_appimage(Some(layout.appimage.clone().into()), appdir(), &sibling),
            None,
            "a folder whose name only starts the same"
        );
    }

    /// GIVEN an empty data folder WHEN the entry is added THEN the three
    /// icons and the exact entry are there, no partial file is left, and
    /// the state is `added`; WHEN removed THEN all four files are gone and
    /// the state is `absent`.
    #[test]
    fn given_no_entry_when_added_and_removed_then_the_files_come_and_go() {
        let layout = Layout::new();
        let places = layout.places();
        assert_eq!(places.state(), AppMenuState::Absent);
        assert_eq!(places.add().unwrap(), AppMenuState::Added);
        assert_eq!(
            fs::read_to_string(layout.entry()).unwrap(),
            app_menu::entry(&places.appimage).unwrap()
        );
        for (size, bytes) in ICONS {
            assert_eq!(fs::read(layout.icon(size)).unwrap(), bytes, "{size}");
        }
        let names: Vec<_> = fs::read_dir(layout.data_home.join("applications"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [DESKTOP_FILE], "no partial file is left");
        assert_eq!(places.add().unwrap(), AppMenuState::Added, "a second add");
        assert_eq!(places.remove().unwrap(), AppMenuState::Absent);
        assert!(!layout.entry().exists());
        for (size, _) in ICONS {
            assert!(!layout.icon(size).exists(), "{size}");
        }
    }

    /// GIVEN Persephone's entry for another AppImage path WHEN added THEN
    /// it is replaced with this path's entry.
    #[test]
    fn given_an_outdated_own_entry_when_added_then_it_is_replaced() {
        let layout = Layout::new();
        let places = layout.places();
        fs::create_dir_all(layout.entry().parent().unwrap()).unwrap();
        fs::write(
            layout.entry(),
            app_menu::entry("/old/Persephone.AppImage").unwrap(),
        )
        .unwrap();
        assert_eq!(places.state(), AppMenuState::Outdated);
        assert_eq!(places.add().unwrap(), AppMenuState::Added);
        assert!(
            fs::read_to_string(layout.entry())
                .unwrap()
                .contains(&format!("\nTryExec={}\n", places.appimage))
        );
    }

    /// GIVEN a package's `Persephone.desktop` in a system data folder WHEN
    /// added THEN the state is `provided` and nothing is written, so the
    /// package's entry is not hidden.
    #[test]
    fn given_a_package_entry_when_added_then_nothing_is_written() {
        let layout = Layout::new();
        let places = layout.places();
        fs::write(
            layout.system.join("applications").join(DESKTOP_FILE),
            "[Desktop Entry]\nName=Persephone\n",
        )
        .unwrap();
        assert_eq!(places.add().unwrap(), AppMenuState::Provided);
        assert!(!layout.data_home.join("applications").exists());
        assert!(!layout.data_home.join("icons").exists());
    }

    /// GIVEN another program's user entry, and separately a link at the
    /// entry's path to a file elsewhere WHEN added or removed THEN the
    /// state is `provided`, the file and the link's target are unchanged,
    /// and no icon is written.
    #[test]
    fn given_another_programs_file_or_a_link_when_changed_then_it_stays() {
        let layout = Layout::new();
        let places = layout.places();
        let applications = layout.data_home.join("applications");
        fs::create_dir_all(&applications).unwrap();
        let theirs = "[Desktop Entry]\nName=Persephone\nExec=gearlever\n";
        fs::write(layout.entry(), theirs).unwrap();
        assert_eq!(places.add().unwrap(), AppMenuState::Provided);
        assert_eq!(places.remove().unwrap(), AppMenuState::Provided);
        assert_eq!(fs::read_to_string(layout.entry()).unwrap(), theirs);
        assert!(!layout.data_home.join("icons").exists());

        fs::remove_file(layout.entry()).unwrap();
        let target = layout.data_home.join("elsewhere.desktop");
        let own = app_menu::entry(&places.appimage).unwrap();
        fs::write(&target, &own).unwrap();
        symlink(&target, layout.entry()).unwrap();
        assert_eq!(places.state(), AppMenuState::Provided, "a link is not read");
        assert_eq!(places.add().unwrap(), AppMenuState::Provided);
        assert_eq!(places.remove().unwrap(), AppMenuState::Provided);
        assert!(fs::symlink_metadata(layout.entry()).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), own);
    }

    /// GIVEN a user entry larger than the read limit, even with the mark,
    /// WHEN read THEN it is another program's.
    #[test]
    fn given_a_large_entry_when_read_then_it_is_not_ours() {
        let layout = Layout::new();
        fs::create_dir_all(layout.entry().parent().unwrap()).unwrap();
        let mut large = app_menu::entry(&layout.places().appimage).unwrap();
        large.push_str(&"#".repeat(MAX_ENTRY_BYTES as usize));
        fs::write(layout.entry(), large).unwrap();
        assert_eq!(read_entry(&layout.entry()), Found::Other);
        assert_eq!(layout.places().state(), AppMenuState::Provided);
    }

    /// GIVEN a file that appears at the entry's path after the check WHEN
    /// the new entry is linked into place THEN it fails with
    /// `AlreadyExists`, the file stays, and no partial file is left.
    #[test]
    fn given_a_file_that_appeared_when_linked_then_it_is_not_replaced() {
        let layout = Layout::new();
        let dir = layout.data_home.join("applications");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(DESKTOP_FILE), "theirs").unwrap();
        let error = put_file(&dir, DESKTOP_FILE, b"ours", false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(dir.join(DESKTOP_FILE)).unwrap(),
            "theirs"
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    /// GIVEN a partial file left by an earlier core with this process ID,
    /// even a link to a file elsewhere, WHEN a file is put THEN the link is
    /// removed, its target is unchanged, and the new file is written.
    #[test]
    fn given_a_left_partial_link_when_put_then_its_target_is_unchanged() {
        let layout = Layout::new();
        let dir = layout.data_home.join("applications");
        fs::create_dir_all(&dir).unwrap();
        let target = layout.data_home.join("victim");
        fs::write(&target, "keep").unwrap();
        let partial = dir.join(format!(".{DESKTOP_FILE}.{}.partial", std::process::id()));
        symlink(&target, &partial).unwrap();
        put_file(&dir, DESKTOP_FILE, b"new", true).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert_eq!(fs::read_to_string(dir.join(DESKTOP_FILE)).unwrap(), "new");
        assert!(fs::symlink_metadata(&partial).is_err());
    }

    /// GIVEN `XDG_DATA_DIRS` values WHEN read THEN only absolute folders
    /// other than the user's own and outside the image's mount folder count,
    /// an unset, empty or wholly relative value gives the XDG default, and
    /// the list is bounded.
    #[test]
    fn given_data_dirs_values_when_read_then_they_follow_the_xdg_rules() {
        let home = Path::new("/home/a/.local/share");
        let appdir = Path::new("/tmp/.mount_Persep");
        let default = vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ];
        assert_eq!(data_dirs(None, home, appdir), default);
        assert_eq!(data_dirs(Some("".into()), home, appdir), default);
        assert_eq!(data_dirs(Some("rel:other".into()), home, appdir), default);
        assert_eq!(
            data_dirs(
                Some("/opt/share:rel:/home/a/.local/share:/usr/share".into()),
                home,
                appdir
            ),
            [PathBuf::from("/opt/share"), PathBuf::from("/usr/share")]
        );
        assert_eq!(
            data_dirs(
                Some("/tmp/.mount_Persep/usr/share:/tmp/.mount_Persepx/share:/usr/share".into()),
                home,
                appdir
            ),
            [
                PathBuf::from("/tmp/.mount_Persepx/share"),
                PathBuf::from("/usr/share")
            ],
            "only folders inside the mount folder are left out"
        );
        let many = vec!["/d"; MAX_DATA_DIRS + 5].join(":");
        assert_eq!(
            data_dirs(Some(many.into()), home, appdir).len(),
            MAX_DATA_DIRS
        );
    }

    /// GIVEN the compiled icons THEN each is a PNG of its folder's size, so
    /// a changed bundle icon cannot put a wrong size in a folder.
    #[test]
    fn given_the_icons_then_each_png_has_its_folders_size() {
        for (size, bytes) in ICONS {
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{size}");
            assert_eq!(&bytes[12..16], b"IHDR", "{size}");
            let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            assert_eq!(format!("{width}x{height}"), size);
        }
    }
}
