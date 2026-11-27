//! The app menu entry on Linux (ADR-0072): the AppImage checks and the file
//! work for [`crate::app_menu`].
//!
//! The entry goes to `$XDG_DATA_HOME/applications/Persephone.desktop` (else
//! `~/.local/share`), and its icons to `icons/hicolor/<size>/apps` there.
//! The icons are the bundle's PNG files, compiled in, so nothing is read
//! from the image or the disk to write them.
//!
//! Persephone writes a file only where no file is, and removes only its own
//! file, so no other program's file is replaced or deleted:
//!
//! * A file is written to a new file with a random name in the same folder
//!   (`O_EXCL`), synced, and hard-linked to its name, which fails when a
//!   file appeared meanwhile. A reader never sees part of a file.
//! * A file is removed (also an outdated entry before its new one) only
//!   after it was renamed to a random name in the same folder and found to
//!   be exactly Persephone's: the entry's exact text ([`app_menu::is_ours`])
//!   or the icon's exact bytes. So the file that is checked is the one that
//!   is removed. Anything else is linked back.
//! * An icon place that holds another file stops the add before anything
//!   is written. When an add ends without Persephone's entry, the icons
//!   that it wrote are removed again.
//!
//! Existing files are read with `O_NOFOLLOW` and a size limit, and no link
//! is followed. The folders above the files are the user's own and may be
//! links (for example from a dotfile manager), as the XDG rules allow. No
//! other program is started.

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

    fn icon_dir(&self, size: &str) -> PathBuf {
        self.hicolor().join(size).join("apps")
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

    /// Writes the missing icons and then the entry, when the state is
    /// `absent` or `outdated`; returns the new state. An icon place that
    /// holds another file stops the add before anything is written
    /// (`AlreadyExists`). When the add ends without Persephone's entry, the
    /// icons that it wrote are removed again.
    fn add(&self) -> io::Result<AppMenuState> {
        let outdated = match self.state() {
            AppMenuState::Absent => false,
            AppMenuState::Outdated => true,
            other => return Ok(other),
        };
        let entry = app_menu::entry(&self.appimage)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let icon = icon_file();
        let mut missing = Vec::new();
        for (size, bytes) in ICONS {
            let dir = self.icon_dir(size);
            match read_file(&dir.join(&icon), bytes.len() as u64) {
                Found::Missing => missing.push((dir, bytes)),
                // Persephone's icon, from an earlier add: it stays.
                Found::Content(content) if content == bytes => {}
                _ => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            }
        }
        let mut created = Vec::new();
        let added = self.put_icons_and_entry(&entry, outdated, missing, &mut created);
        if !matches!(added, Ok(true)) {
            for (dir, bytes) in &created {
                // An icon that another program put there meanwhile stays.
                let _ = take_own(dir, &icon, bytes.len() as u64, |content| content == *bytes);
            }
        }
        added?;
        Ok(self.state())
    }

    /// Writes each `missing` icon, records it in `created`, and then the
    /// entry, after it takes away the outdated one. Returns whether the
    /// entry was written.
    fn put_icons_and_entry(
        &self,
        entry: &str,
        outdated: bool,
        missing: Vec<(PathBuf, &'static [u8])>,
        created: &mut Vec<(PathBuf, &'static [u8])>,
    ) -> io::Result<bool> {
        let icon = icon_file();
        for (dir, bytes) in missing {
            make_dirs(&dir)?;
            put_file(&dir, &icon, bytes)?;
            created.push((dir, bytes));
        }
        // GTK and Qt skip an `icon-theme.cache` that is older than its
        // theme folder; one there would not list the new icons.
        let _ = File::open(self.hicolor()).and_then(|dir| dir.set_modified(SystemTime::now()));
        let applications = self.applications();
        make_dirs(&applications)?;
        if outdated
            && !take_own(
                &applications,
                DESKTOP_FILE,
                MAX_ENTRY_BYTES,
                app_menu::is_ours,
            )?
        {
            // The entry changed after the check and is not Persephone's.
            return Ok(false);
        }
        match put_file(&applications, DESKTOP_FILE, entry.as_bytes()) {
            Ok(()) => Ok(true),
            // A file appeared after the check: it is not replaced.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Removes the entry when it is Persephone's own, and then the icons
    /// that are exactly Persephone's; returns the new state. Another
    /// program's file is not touched.
    fn remove(&self) -> io::Result<AppMenuState> {
        let applications = self.applications();
        if let Found::Content(content) = read_entry(&applications.join(DESKTOP_FILE))
            && app_menu::is_ours(&content)
            && take_own(
                &applications,
                DESKTOP_FILE,
                MAX_ENTRY_BYTES,
                app_menu::is_ours,
            )?
        {
            let icon = icon_file();
            for (size, bytes) in ICONS {
                take_own(&self.icon_dir(size), &icon, bytes.len() as u64, |content| {
                    content == bytes
                })?;
            }
        }
        Ok(self.state())
    }
}

fn icon_file() -> String {
    format!("{ICON_NAME}.png")
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

/// The absolute folders among the first [`MAX_DATA_DIRS`] entries of
/// `$XDG_DATA_DIRS` (else the XDG default), without the user's own data
/// folder and without the folders inside the image's mount folder
/// `appdir`. The AppImage's GTK hook puts `$APPDIR/usr/share` first in
/// this process's list; the image's own `Persephone.desktop` there is not
/// in the desktop's menu. Entries after the limit are not split or kept.
fn data_dirs(value: Option<OsString>, data_home: &Path, appdir: &Path) -> Vec<PathBuf> {
    let listed: Vec<PathBuf> = value
        .filter(|value| !value.is_empty())
        .map(|value| {
            env::split_paths(&value)
                .take(MAX_DATA_DIRS)
                .filter(|dir| dir.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let dirs = if listed.is_empty() {
        vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]
    } else {
        listed
    };
    dirs.into_iter()
        .filter(|dir| dir != data_home && !dir.starts_with(appdir))
        .collect()
}

/// What is at `path`, read without following a link, at most
/// [`MAX_ENTRY_BYTES`].
fn read_entry(path: &Path) -> Found {
    read_file(path, MAX_ENTRY_BYTES)
}

/// What is at `path`, read without following a link, at most `limit` bytes.
fn read_file(path: &Path, limit: u64) -> Found {
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
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return Found::Other;
    }
    let mut content = Vec::new();
    // Bounded again while it is read: the file can grow meanwhile.
    match file.take(limit + 1).read_to_end(&mut content) {
        Ok(read) if read as u64 <= limit => Found::Content(content),
        _ => Found::Other,
    }
}

/// Makes the missing folders, for the user alone: only the user's own
/// desktop reads them, and the entry names the AppImage's path.
fn make_dirs(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Puts `bytes` at `dir/name` only where no file is: a new file with a
/// random name next to it (`O_EXCL`), synced, then hard-linked to the name,
/// which fails with `AlreadyExists` when a file is there. The random name
/// is removed in every case; no existing file is ever removed to make room.
/// The file is for the user alone, as in [`make_dirs`].
fn put_file(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let partial = dir.join(random_name(name, "partial")?);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::hard_link(&partial, dir.join(name)));
    let _ = fs::remove_file(&partial);
    result?;
    sync_dir(dir);
    Ok(())
}

/// Removes `dir/name` only when it is Persephone's own file. The file (or
/// link) is first renamed to a random name in the same folder, so the file
/// that `own` checks is the one that is removed, also when another program
/// writes the name meanwhile. A file that is not `own` is linked back; when
/// that fails (another file took the name meanwhile), it is kept under the
/// random name, and the error is returned. A folder is not moved, because
/// it could not be linked back. Returns whether an own file was removed; a
/// missing file is `false`.
fn take_own(dir: &Path, name: &str, limit: u64, own: impl Fn(&[u8]) -> bool) -> io::Result<bool> {
    let path = dir.join(name);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(false);
    }
    let aside = dir.join(random_name(name, "old")?);
    match fs::rename(&path, &aside) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        result => result?,
    }
    if matches!(read_file(&aside, limit), Found::Content(content) if own(content.as_slice())) {
        fs::remove_file(&aside)?;
        sync_dir(dir);
        return Ok(true);
    }
    // `linkat` without `AT_SYMLINK_FOLLOW`: a link goes back as a link.
    fs::hard_link(&aside, &path)?;
    fs::remove_file(&aside)?;
    Ok(false)
}

/// `.{name}.{32 random hex digits}.{kind}`: a hidden name that no existing
/// file has, and that no desktop or icon reader takes for an entry or an
/// icon, since it does not end in `.desktop` or `.png`.
fn random_name(name: &str, kind: &str) -> io::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("no random bytes"))?;
    let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(".{name}.{hex}.{kind}"))
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
        assert_eq!(places.data_dirs, std::slice::from_ref(&layout.system));
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

    /// The names in a folder, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// GIVEN Persephone's entry for another AppImage path WHEN added THEN
    /// it is replaced with this path's entry, and no set-aside or partial
    /// file is left.
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
        assert_eq!(names(layout.entry().parent().unwrap()), [DESKTOP_FILE]);
    }

    /// GIVEN another file at one icon place WHEN added THEN the add fails
    /// with `AlreadyExists` before anything is written: that file is
    /// unchanged, and no other icon and no entry is there.
    #[test]
    fn given_another_icon_when_added_then_nothing_is_written() {
        let layout = Layout::new();
        let places = layout.places();
        let theirs = layout.icon("128x128");
        fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        fs::write(&theirs, b"another program's icon").unwrap();
        let error = places.add().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&theirs).unwrap(), b"another program's icon");
        assert!(!layout.icon("32x32").exists());
        assert!(!layout.icon("256x256").exists());
        assert!(!layout.entry().exists());
        assert_eq!(places.state(), AppMenuState::Absent);
    }

    /// GIVEN Persephone's icon already at one place WHEN added and removed
    /// THEN it is kept by the add and removed with the others.
    #[test]
    fn given_an_own_icon_when_added_then_it_is_kept() {
        let layout = Layout::new();
        let places = layout.places();
        let (size, bytes) = ICONS[0];
        fs::create_dir_all(layout.icon(size).parent().unwrap()).unwrap();
        fs::write(layout.icon(size), bytes).unwrap();
        assert_eq!(places.add().unwrap(), AppMenuState::Added);
        assert_eq!(fs::read(layout.icon(size)).unwrap(), bytes);
        assert_eq!(places.remove().unwrap(), AppMenuState::Absent);
        assert!(!layout.icon(size).exists());
    }

    /// GIVEN an added entry whose icon another program then replaced, and
    /// one icon that it replaced with a link, WHEN removed THEN the entry
    /// and the remaining own icon go, the other file and the link stay as
    /// they are, and no set-aside file is left.
    #[test]
    fn given_replaced_icons_when_removed_then_they_stay() {
        let layout = Layout::new();
        let places = layout.places();
        assert_eq!(places.add().unwrap(), AppMenuState::Added);
        fs::write(layout.icon("32x32"), b"another program's icon").unwrap();
        let target = layout.data_home.join("elsewhere.png");
        fs::write(&target, ICONS[1].1).unwrap();
        fs::remove_file(layout.icon("128x128")).unwrap();
        symlink(&target, layout.icon("128x128")).unwrap();
        assert_eq!(places.remove().unwrap(), AppMenuState::Absent);
        assert!(!layout.entry().exists());
        assert!(!layout.icon("256x256").exists());
        assert_eq!(
            fs::read(layout.icon("32x32")).unwrap(),
            b"another program's icon"
        );
        assert!(
            fs::symlink_metadata(layout.icon("128x128"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), ICONS[1].1);
        for size in ["32x32", "128x128"] {
            assert_eq!(
                names(layout.icon(size).parent().unwrap()),
                [format!("{ICON_NAME}.png")]
            );
        }
    }

    /// GIVEN an applications folder that cannot be written WHEN added THEN
    /// the add fails, and the icons that it wrote are removed again.
    #[test]
    fn given_an_entry_that_cannot_be_written_when_added_then_the_new_icons_go() {
        let layout = Layout::new();
        let places = layout.places();
        let applications = layout.data_home.join("applications");
        fs::create_dir_all(&applications).unwrap();
        fs::set_permissions(&applications, fs::Permissions::from_mode(0o555)).unwrap();
        // Root (or `CAP_DAC_OVERRIDE`) may write anyway: there is no failure
        // to test then.
        if fs::write(applications.join("probe"), b"").is_ok() {
            fs::set_permissions(&applications, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let result = places.add();
        fs::set_permissions(&applications, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        for (size, _) in ICONS {
            assert!(!layout.icon(size).exists(), "{size}");
            assert!(
                names(layout.icon(size).parent().unwrap()).is_empty(),
                "{size}"
            );
        }
        assert_eq!(places.state(), AppMenuState::Absent);
    }

    /// GIVEN a file that is not Persephone's, a link, and Persephone's own
    /// file WHEN each is taken THEN only the own file is removed; the other
    /// file and the link are back under their name, unchanged, and no
    /// set-aside name is left.
    #[test]
    fn given_files_when_taken_then_only_the_own_file_goes() {
        let layout = Layout::new();
        let dir = layout.data_home.clone();
        let own = |content: &[u8]| content == b"own";
        fs::write(dir.join("a"), b"theirs").unwrap();
        assert!(!take_own(&dir, "a", 16, own).unwrap());
        assert_eq!(fs::read(dir.join("a")).unwrap(), b"theirs");
        fs::write(dir.join("target"), b"own").unwrap();
        symlink(dir.join("target"), dir.join("b")).unwrap();
        assert!(!take_own(&dir, "b", 16, own).unwrap(), "a link is not read");
        assert!(fs::symlink_metadata(dir.join("b")).unwrap().is_symlink());
        fs::write(dir.join("c"), b"own").unwrap();
        assert!(take_own(&dir, "c", 16, own).unwrap());
        assert!(!take_own(&dir, "missing", 16, own).unwrap());
        fs::create_dir(dir.join("d")).unwrap();
        assert!(!take_own(&dir, "d", 16, own).unwrap(), "a folder");
        assert!(dir.join("d").is_dir());
        assert_eq!(names(&dir), ["a", "b", "d", "target"]);
        assert_eq!(fs::read(dir.join("target")).unwrap(), b"own");
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
        let error = put_file(&dir, DESKTOP_FILE, b"ours").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(dir.join(DESKTOP_FILE)).unwrap(),
            "theirs"
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    /// GIVEN other files in the folder, among them one and a link with the
    /// partial name of this process that an earlier version used, WHEN a
    /// file is put THEN the new file is written under its name, the other
    /// files stay as they are, and no new name is left.
    #[test]
    fn given_other_files_when_put_then_none_is_removed_or_replaced() {
        let layout = Layout::new();
        let dir = layout.data_home.join("applications");
        fs::create_dir_all(&dir).unwrap();
        let target = layout.data_home.join("victim");
        fs::write(&target, "keep").unwrap();
        let old_partial = format!(".{DESKTOP_FILE}.{}.partial", std::process::id());
        symlink(&target, dir.join(&old_partial)).unwrap();
        fs::write(dir.join("other.desktop"), "theirs").unwrap();
        put_file(&dir, DESKTOP_FILE, b"new").unwrap();
        assert_eq!(fs::read_to_string(dir.join(DESKTOP_FILE)).unwrap(), "new");
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert!(
            fs::symlink_metadata(dir.join(&old_partial))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(dir.join("other.desktop")).unwrap(),
            "theirs"
        );
        let mut expected = vec![
            old_partial,
            DESKTOP_FILE.to_owned(),
            "other.desktop".to_owned(),
        ];
        expected.sort();
        assert_eq!(names(&dir), expected);
        let first = random_name(DESKTOP_FILE, "partial").unwrap();
        assert_ne!(first, random_name(DESKTOP_FILE, "partial").unwrap());
        assert!(first.starts_with(".Persephone.desktop.") && first.ends_with(".partial"));
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
        let late = format!("{}/opt/share", "rel:".repeat(MAX_DATA_DIRS));
        assert_eq!(
            data_dirs(Some(late.into()), home, appdir),
            default,
            "an entry after the limit is not looked at"
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
