# Offer a desktop entry for the Linux AppImage

* Status: proposed
* Deciders: project owner (asked for the AppImage's own icon on KDE Plasma; this record needs the owner's approval of the choices below)
* Date: 2026-11-27

Technical Story: on KDE Plasma (Wayland), the 0.3.0 AppImage shows the
generic `wayland` icon in the KWin title bar and the task manager when no
integration tool installed it. Dolphin also shows no icon for the AppImage
file. Pull request 111 (now in `main`) sets the GLib program name to the
product name, so the
Wayland app ID is `Persephone`, which matches the `Persephone.desktop` that
every Linux package installs. That fixes the installed packages. This record
covers the AppImage that nothing installed.

## Context and Problem Statement

KWin finds the icon of a Wayland window in two ways only
(`XdgToplevelWindow::updateIcon` and `Window::findDesktopFile` in KWin's
`master`, read on 2026-11-27):

* an icon that the client sends with the `xdg-toplevel-icon` protocol. GTK3
  never sends one, and GTK3 does not give a program its `xdg_toplevel`
  object, so the UI host cannot send one either;
* the `Icon=` key of `<app ID>.desktop`, found with
  `QStandardPaths::locate(ApplicationsLocation, …)`, that is in
  `$XDG_DATA_HOME/applications` or an `applications` folder of
  `$XDG_DATA_DIRS`. KWin then calls `QIcon::fromTheme` with that name. There
  is no fallback to `StartupWMClass`.

An AppImage carries a desktop file, but only inside its own image, where no
desktop looks. So an AppImage that nothing installed has no
`Persephone.desktop` in any applications folder, and KWin shows `wayland`.

The integration tools do not help KWin either, because they write other
file names (sources read on 2026-11-27):

* AppImageLauncher, through libappimage's `Integrator.cpp`, writes
  `appimagekit_<id>-Persephone.desktop`.
* Gear Lever (`AppImageProvider.py`) writes the lower-case name with spaces
  changed to underscores: `persephone.desktop`.

Neither is `Persephone.desktop`, so KWin does not find them. Other parts of
Plasma (the task manager) may still match them by other keys.

How can the AppImage show its own icon in KWin, the task manager and the app
menu without an integration tool, and can Dolphin's file icon be fixed?

## Decision Drivers

* The core owns every file write (ADR-0004). The UI host and the page stay
  keyless and get no file system access.
* Nothing is written outside the profile without the user's yes in the
  core's own window. "No" is the default, as for the other questions.
* Treat the path of the AppImage, the existing files and the environment as
  untrusted input: a file name can come from a web download.
* Fail closed: write nothing when the AppImage cannot be checked, when the
  path cannot be written safely into a desktop entry, or when another
  program's entry is already there.
* Do not weaken client isolation. Do not start other programs.
* Measure the cost of a release change before making it.

## Considered Options

1. The core adds a user desktop entry for the AppImage when the user asks in
   Settings, after the core's own question.
2. Leave integration to AppImageLauncher, Gear Lever and similar tools, and
   document it.
3. Ask at every start, from the core, before the window opens.
4. Force XWayland (`GDK_BACKEND=x11`) for the AppImage.
5. Build the AppImage with gzip squashfs instead of zstd, for Dolphin's
   thumbnailer.

## Decision Outcome

Chosen option: "1. The core adds a user desktop entry when the user asks",
because it is the only option that gives KWin the file it looks for. Option 5
is rejected: it has a measured cost and no verified benefit (see below). The
owner must approve this record before it is accepted.

The entry works for KWin only with the program name of pull request 111,
because KWin looks for `<app ID>.desktop`. That change is in `main`. Without
it the app ID is `persephone-ui`, and the entry adds Persephone to the app
menu only.

### When the page offers it

The core checks that it runs from an AppImage. All of these must be true, or
the state is `unavailable` and the page shows nothing:

* The operating system is Linux.
* `APPIMAGE` is set, valid UTF-8, absolute, at most 4096 bytes, and equal to
  its own canonical path (no `.` or `..` part, no symbolic link). The type 2
  runtime sets it from `/proc/self/exe`, so it is canonical.
* The path has no control character, no bidirectional formatting character
  and no white space at its end. The question shows the path, so it must
  read as it is stored, and a desktop entry cannot hold a control character
  safely.
* The file, opened without following a link, is a regular file with an
  execute bit, starts with the ELF magic, and has the AppImage type 2 magic
  `AI\x02` at byte 8.
* `APPDIR` is set, absolute and canonical, and the core's own executable is
  inside it. So the process runs from a mounted AppImage, not from a package
  with a stray `APPIMAGE` value.

The panel "App menu" in Settings, under "This session", then shows one of
these states:

* `absent`: no `Persephone.desktop` in any applications folder. The page
  offers "Add to app menu".
* `added`: Persephone's own entry for this AppImage. The page offers
  "Remove from app menu".
* `outdated`: Persephone's own entry, but for another path. The page offers
  to update or remove it.
* `provided`: a `Persephone.desktop` that Persephone did not write: a
  package in an `$XDG_DATA_DIRS` folder, or a user file that is not exactly
  Persephone's text (also an edited copy, a link or a file over 16 KiB).
  The page offers nothing. A user entry with the same desktop ID would hide
  the package's entry, so the menu would start the AppImage instead of the
  package.

A user file is Persephone's own only when it is exactly the text below for
the path in its own `TryExec` line. The `X-Persephone-AppImage=true` line
alone is not enough, so the core never replaces or removes a copy that the
user or another program changed. A later version that changes the text must
also accept the earlier exact texts.

This is a check of content, not of origin. A file with exactly this text,
or an icon with exactly Persephone's bytes at `persephone-appimage.png`, is
treated as Persephone's, whoever wrote it: "Update" replaces it and "Remove"
removes it. No record could prove the origin better. Every program that runs
as the user can write, change and delete these files, and it could write
such a record in the user's folders as well. A byte-identical file is the
same entry or icon, so removing it is what the user asked for. A file that
differs in any byte is never replaced or removed.

The folders inside `APPDIR` do not count. The AppImage's GTK hook
(`apprun-hooks/linuxdeploy-plugin-gtk.sh`) puts `$APPDIR/usr/share` first in
the process's `XDG_DATA_DIRS`. The image's own `Persephone.desktop` is there,
but the desktop does not read that folder, so it is not in the menu.

### The question and the write

"Add" (and "Update") asks in the core's own window first. The window shows
the AppImage's path and the two folders. "No" is the default; closing the
window is "No". After a yes, the core checks the state again, because the
question can stay open for minutes, and writes only when it is still
`absent` or `outdated`.

The core then writes, under `$XDG_DATA_HOME` (else `~/.local/share`):

* the icons `icons/hicolor/{32x32,128x128,256x256}/apps/persephone-appimage.png`,
  first. They are the bundle's PNG files, compiled into the core, so no
  file is read from the image or the disk. The name is not the packages'
  `persephone-ui`. An icon is written only where no file is, and one that
  is already exactly Persephone's icon stays. Another file at one of the
  three places stops the add before anything is written, and the page
  shows that the entry could not be changed. The core then sets the time of
  the `hicolor` folder, so an out-of-date `icon-theme.cache` there is not
  used;
* then `applications/Persephone.desktop`:

  ```ini
  [Desktop Entry]
  Type=Application
  Name=Persephone
  Comment=Matrix desktop client
  Exec="<the AppImage path, quoted>"
  TryExec=<the AppImage path>
  Icon=persephone-appimage
  StartupWMClass=Persephone
  Categories=Network;
  Terminal=false
  X-Persephone-AppImage=true
  ```

  The path in `Exec` is quoted as the Desktop Entry Specification requires:
  the quote character, the back quote, the dollar sign and the backslash get
  a backslash, `%` becomes `%%`, and then every backslash is escaped again
  for the string value. A path with a control character is refused, not
  escaped, so no path can start a new line or key. `TryExec` makes menus
  hide the entry when the AppImage is no longer there; KWin still finds the
  file and its icon.

The core writes a file only where no file is, and removes only a file that
is exactly its own:

* Each file is written to a new file with a random name in the same folder
  (`O_EXCL`, mode `0600`; new folders get `0700`, because only the user's
  own desktop reads them and the entry names the AppImage's path), synced,
  and hard-linked to its name. The link
  fails when a file appeared there meanwhile, and that file stays. So a
  reader never sees part of a file, and no file is replaced.
* A file is removed (also an outdated entry, before its new one is written)
  only after it was renamed to a random name in the same folder and found
  to be exactly Persephone's: the exact entry text, or the exact icon bytes.
  So the file that is checked is the one that is removed, also when another
  program writes the name meanwhile. Anything else is linked back under its
  name. A folder at the name is not moved.
* When an add ends without Persephone's entry (a failed write, or a file
  that appeared at the entry's name), the icons that this add wrote are
  removed again in the same way.

Existing files are read with `O_NOFOLLOW` and a size limit (16 KiB for the
entry, the icon's own size for an icon), and no link is followed. The core
makes the folders when they are missing. It does not run
`update-desktop-database`, `kbuildsycoca6` or any other program; the
desktops watch these folders. A window that is already open keeps the generic
icon until Persephone starts again, because KWin reads the icon when the
window gets its app ID.

"Remove" asks no question: it removes `Persephone.desktop` when it is
exactly Persephone's, and then each `persephone-appimage.png` icon that is
exactly Persephone's. It never removes a package's file or another
program's file, also one that replaced Persephone's file.

### Dolphin's file icon

Dolphin's AppImage thumbnailer (`kio-extras`, `appimagecreator.cpp`) reads
`.DirIcon` with libappimage's
`appimage_read_file_into_buffer_following_symlinks`. libappimage's last tag
is v1.0.4-5 (11 May 2022), and its own copy of squashfuse got zstd support
only on `master` (commit 44e4db94d5, 25 November 2024). But distributions
can build it with their own squashfuse: Ubuntu 24.04's
`libappimage1.0abi1t64` 1.0.4-5-3.1ubuntu2 links the system
`libsquashfuse0` 0.5.0, which depends on libzstd, and Arch's PKGBUILD uses
`-DUSE_SYSTEM_SQUASHFUSE=ON`. A test program with Ubuntu's library read the
`.DirIcon` of the shipped zstd AppImage (see
[the measurement](../evidence/appimage-icon.md)). KIO and Dolphin enable
every thumbnailer except the text one by default.

So zstd is not the cause on Ubuntu-based systems, and probably not on Arch.
A gzip image costs 12.8 MB (11.0 %) more and takes twice as long to unpack
(0.78 s to 1.56 s for the whole image), and no system where it helps is
known. The AppImage stays zstd, and the release check of the compression is
not added. The cause on the owner's KDE system is still open. These are the
next checks there: whether that distribution's `kio-extras` has the AppImage
thumbnailer and builds libappimage with a squashfuse that reads zstd, whether
"AppImage" is on in Dolphin's preview settings, and whether
`~/.cache/thumbnails/fail` holds an old failure for the file. The desktop
entry does not change Dolphin's file icon.

### Positive Consequences

* KDE Plasma on Wayland shows Persephone's icon in the title bar, the task
  manager and the app menu for an AppImage that nothing installed. This
  needs the program name of pull request 111, which is in `main`.
* The menu entry and icons follow the XDG rules, so GNOME and other
  desktops that look up `<app ID>.desktop` show the icon too.
* The page can only ask. The write, its checks and the question are in the
  core.

### Negative Consequences

* The core gets a write path outside the profile, with its own tests and
  review.
* A user entry hides a package's `Persephone.desktop` while both exist. The
  page shows the state, and Remove clears it.
* An entry for a moved or deleted AppImage stays until the user updates or
  removes it. Its icon still works, and `TryExec` hides it from menus.
* The core binary carries about 14 KiB of PNG icons on Linux.
* Another file named `persephone-appimage.png` in the user's icon folders
  stops the add, and an entry that the user edited can no longer be updated
  or removed from Settings. Both are left to the user.
* Ownership is by exact content. A byte-identical entry or icon that another
  program wrote is updated or removed as Persephone's.
* A file system without hard links cannot take the entry; the add then
  fails and writes nothing that stays.
* AppImageLauncher or Gear Lever can still add their own entries beside it.
  The menu can then list Persephone twice.

## Pros and Cons of the Options

### 1. The core adds a user desktop entry when the user asks

* Good, because it writes the one file name that KWin looks for.
* Good, because the user starts it in Settings, so there is no question at
  start and no "do not ask again" state to store.
* Good, because the page gets two narrow commands, `app_menu_status` and
  `set_app_menu { on }`, and the core does the checks and the question.
* Bad, because it is new code that writes outside the profile.

### 2. Leave it to the integration tools

* Good, because Persephone writes nothing outside its profile.
* Bad, because the tools write `appimagekit_<id>-Persephone.desktop` and
  `persephone.desktop`, which KWin does not find. The title-bar icon stays
  generic with both tools.
* Bad, because many users have no such tool.

### 3. Ask at every start

* Good, because users find it without opening Settings.
* Bad, because the core opens the profile before any window exists, so the
  question would come before the window, next to the keyring and passphrase
  questions. Storing a "no" needs a file outside the profile that is read
  before the profile is unlocked.
* Bad, because the user still needs a way to remove the entry, which needs
  the Settings panel anyway.

### 4. Force XWayland

* Good, because KWin then finds the icon through `WM_CLASS`.
* Bad, because X11 gives every X client access to the other windows' input
  and contents. That weakens client isolation and the capture protection.
  Rejected.

### 5. gzip squashfs

* Good, because libappimage builds that use their own old squashfuse can read
  gzip.
* Bad, because it costs 12.8 MB (11.0 %) per x64 AppImage and doubles the
  unpack time, measured on the 0.3.0 image.
* Bad, because the libappimage that Ubuntu ships reads the zstd image
  already, so the cost has no verified benefit.

## Links

* Depends on pull request 111, merged to `main` (program name `Persephone`,
  `linux/main.desktop` with `StartupWMClass`).
* Uses the core question pattern of [ADR-0024](0024-open-links-only-after-the-core-asks-in-its-own-window.md)
  and [ADR-0058](0058-write-a-diagnostic-log-only-when-the-user-turns-it-on.md).
* Keeps the boundary of [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md).
* Linux packages: [ADR-0012](0012-sign-and-publish-releases-as-drafts.md),
  [ADR-0069](0069-build-an-arch-linux-package-from-the-deb.md).
* Measurement: [AppImage icon and compression](../evidence/appimage-icon.md).
