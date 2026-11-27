# AppImage icon and squashfs compression

Date: 2026-11-27. Supports [ADR-0072](../adr/0072-offer-a-desktop-entry-for-the-linux-appimage.md).

## Question

Would building the Linux AppImage with gzip squashfs instead of zstd let
KDE's Dolphin show the AppImage's icon, and what would it cost?

## Input

* `Persephone_0.3.0_amd64-unsigned.AppImage` from the `latest` pre-release
  (built from f17c36d), 117,152,248 bytes, SHA-256
  `372ff6ecd5cfd3e3d4cb0ca402b68d88b6e89f8c29be70f3686cf9316a86e30c`.
* Linux Mint 22.3 (Ubuntu 24.04 base), kernel 7.0.0-44, 8 CPUs, squashfs-tools
  4.6.1.

The AppImage was not run to find its image. A Python script read the ELF64
header (`e_shoff + e_shentsize * e_shnum`) for the offset of the squashfs
image. The type 2 runtime ran only for `--appimage-extract` in the read test
below.

## Observed

The shipped image (superblock at offset 944,632):

| Field | Value |
| --- | --- |
| Runtime | 944,632 bytes, AppImage magic `AI\x02` at byte 8 |
| Squashfs | 116,206,959 bytes |
| Compression id (superblock offset 20) | 6 (zstd) |
| Block size | 131,072 bytes |
| Files | 376, 378,351,159 bytes unpacked |

The image's root holds `.DirIcon` → `Persephone.png` (256 × 256 PNG, 8,423
bytes), `Persephone.desktop` with `Icon=persephone-ui`, and
`usr/share/icons/hicolor/{32x32,128x128,256x256,256x256@2}/apps/persephone-ui.png`.
`usr/share/applications/Persephone.desktop` is in the image too.

The image's `apprun-hooks/linuxdeploy-plugin-gtk.sh` (read with `grep`):

* line 14 keeps the runtime's `APPDIR`;
* line 17, `export GDK_BACKEND=x11`, is commented out, so GTK uses Wayland
  when the desktop has it;
* line 18 exports `XDG_DATA_DIRS="$APPDIR/usr/share:/usr/share:$XDG_DATA_DIRS"`.

So the core's own `XDG_DATA_DIRS` starts with a folder that holds a
`Persephone.desktop` that the desktop does not read. ADR-0072 leaves folders
inside `APPDIR` out of its check for that reason.

The unpacked tree packed again with `mksquashfs <tree> <out> <options>
-root-owned -noappend`, then unpacked with `unsquashfs -p 1` (one CPU) to
`/dev/shm`:

| Options | Bytes | Unpack time |
| --- | --- | --- |
| `-comp zstd` (128 KiB blocks, as shipped) | 116,207,616 | 0.72 s |
| `-comp zstd -b 1M` | 117,837,824 | 0.44 s |
| `-comp gzip` (128 KiB blocks) | 129,007,616 | 1.17 s |
| `-comp gzip -b 1M` | 127,463,424 | 1.38 s |
| `-comp xz -b 1M` | 97,234,944 | 5.97 s |
| The shipped image | 116,206,959 | 0.81 s |

The zstd row reproduces the shipped image within 657 bytes. gzip at the same
block size is 12,800,000 bytes (11.0 %) larger.

The shipped runtime joined to each repacked image, `--appimage-extract` of
the whole image (one run each): zstd 0.78 s, gzip 1.56 s. Both gave 376 files
and a 256 × 256 PNG `.DirIcon`.

### Ubuntu's libappimage

Ubuntu 24.04's packages were fetched with `apt-get download` and unpacked
with `dpkg-deb -x` into a private folder; nothing was installed:
`libappimage1.0abi1t64` and `libappimage-dev` 1.0.4-5-3.1ubuntu2,
`libsquashfuse0` 0.5.0-2build1, `libxdgutilsbasedir1.0.1` and
`libxdgutilsdesktopentry1.0.1` 1.0.1-3.1build1, `libboost-filesystem1.83.0`
1.83.0-2.1ubuntu3.2. `ldd` showed the test program linked to that
`libsquashfuse.so.0`, which loads the system `libzstd.so.1`. Ubuntu's
`kio-extras` depends on `libappimage1.0abi1t64`.

A C program called the function that KDE's thumbnailer calls,
`appimage_read_file_into_buffer_following_symlinks(path, ".DirIcon", …)`:

| Image | `appimage_get_type` | Read | Bytes | PNG |
| --- | --- | --- | --- | --- |
| The shipped zstd AppImage | 2 | yes | 8,423 | yes |
| Runtime + zstd repack | 2 | yes | 8,423 | yes |
| Runtime + gzip repack | 2 | yes | 8,423 | yes |

## Sources read

* libappimage: last tag v1.0.4-5 (11 May 2022); commit 44e4db94d5 "Update
  squashfuse and add support for Zstd" (25 November 2024, `master` only).
  At v1.0.4-5 its own squashfuse build uses `--without-lzo --without-lz4
  --with-xz`.
* Arch Linux `libappimage` PKGBUILD (1.0.4.5): `-DUSE_SYSTEM_SQUASHFUSE=ON`.
  Not run.
* `kio-extras` `thumbnail/appimagecreator.cpp`: reads `.DirIcon` with the
  function above.
* KIO `PreviewJob::defaultPlugins()`: every thumbnailer except
  `textthumbnail`. Dolphin uses that list and no local size limit by default.
* KWin `Window::findDesktopFile` and `XdgToplevelWindow::updateIcon`, and the
  desktop file names of AppImageLauncher (libappimage `Integrator.cpp`) and
  Gear Lever (`AppImageProvider.py`), as summarised in ADR-0072.

## Result and limits

* On Ubuntu 24.04, the library that Dolphin's thumbnailer uses reads the zstd
  AppImage's icon. gzip would not change Dolphin there.
* gzip costs 12.8 MB per x64 AppImage and doubles the unpack time. The arm64
  image was not measured.
* No KDE desktop was run. The Dolphin failure that the owner saw was not
  reproduced, and its cause is not known. Other distributions, and Arch's
  squashfuse build, were not tested.
* Times are single runs on a warm page cache in a VM. They show the ratio,
  not start times.
