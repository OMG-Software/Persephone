# Build an Arch Linux package from the x86_64 .deb

* Status: proposed
* Deciders: project owner (asked for an Arch Linux package; this record needs the owner's approval of the choices below)
* Date: 2026-11-26

Technical Story: the owner asked for an Arch Linux package among the build
artifacts, so that Arch Linux users do not have to use the AppImage, which
should be a last resort. This record adds a target to
[ADR-0012](0012-sign-and-publish-releases-as-drafts.md).

## Context and Problem Statement

ADR-0012 makes an AppImage, a `.deb` and an `.rpm` for Linux x64 and arm64.
pacman installs none of the native packages, so Arch Linux users have only
the AppImage. The AppImage carries its own copies of GTK and WebKitGTK, which
get security fixes only with a new Persephone build. pacman does not track
it, and it has no desktop entry unless the user adds one. How should the
release workflow make a package for Arch Linux?

## Decision Drivers

* The Arch package has the same programs as the other Linux packages. The
  core, the workers and libwebrtc are not built a second time.
* GTK, WebKitGTK and the other libraries come from Arch, so their security
  fixes reach users through pacman.
* The job has no secrets and adds no new third-party build tool.
* A current Arch Linux checks the package when it is built.
* pacman orders the package versions as the release versions are ordered.

## Considered Options

* Repackage the x86_64 `.deb` with makepkg in an Arch Linux container.
* Build from source in an Arch Linux container.
* Publish a PKGBUILD in the AUR that downloads the released `.deb`.
* Make no Arch package, and document the AppImage.

## Decision Outcome

Chosen option: "Repackage the x86_64 `.deb` with makepkg in an Arch Linux
container", because it ships the programs of the `.deb` unchanged, adds a
short job, and uses the libraries that Arch provides.

* **Job.** `arch-package` in `.github/workflows/release.yml` runs after
  `linux`. It uses `archlinux:base-devel` pinned by the same digest as the
  `arch` job of `check.yml`, and does a full upgrade (`pacman -Syu`) first.
  It has no secrets. `checksums` waits for it and publishes the package.
* **PKGBUILD.** `python tools/release.py arch-pkgbuild` takes the one `.deb`
  in `.local/release/arch/in`, writes `.local/release/arch/build/PKGBUILD`
  from [`tools/arch/PKGBUILD.in`](../../tools/arch/PKGBUILD.in) and copies the
  `.deb` beside it. Like `append-notice`, it takes no file arguments, so no
  argument can send it to another path. The version comes from
  `src-tauri/tauri.conf.json`. The command refuses a `.deb` whose name is not
  the x86_64 `.deb` of that version. The PKGBUILD names the `.deb`'s
  SHA-256, and makepkg checks it.
* **Same files.** `package()` unpacks the `.deb`'s `data.tar` and nothing
  else. `options` sets each makepkg option that changes files (`!strip`,
  `!debug`, `!zipman`, `!purge`, `docs`, `emptydirs`, `libtool`,
  `staticlibs`), so `makepkg.conf` cannot change them. The job compares the
  package's files with the `.deb`'s, by contents and by type, mode, owner,
  group and link target, and fails on any difference.
* **Dependencies.** `depends` lists the package of every library that the
  four programs link (their ELF `NEEDED` entries): `cairo`, `gdk-pixbuf2`,
  `glib2`, `glibc`, `gtk3`, `libgcc`, `libpipewire`, `libsoup3` and
  `webkit2gtk-4.1`. It also lists `libayatana-appindicator` (the tray
  library, loaded at run time), `hicolor-icon-theme` (it holds the icons) and
  `org.freedesktop.secrets`. The core does not open a profile without a
  Secret Service ([ADR-0010](0010-linux-platform-layer.md)), so this is a
  dependency, not an option. Any provider satisfies it, for example
  `gnome-keyring`, `kwallet` or `keepassxc`. `optdepends` lists `pipewire`
  for calls and `xdg-desktop-portal` for links, browser sign-in and screen
  sharing. The job installs the package with `pacman -U`. It fails if a
  linked library belongs to a package that `depends` does not list, or if
  `ldd` cannot find a library.
* **Architecture.** x86_64 only. Arch Linux supports only x86_64. Arch
  Linux ARM is a separate project with no official container image.
* **Version.** A pkgver cannot hold a hyphen. The package drops it
  (`0.1.0-alpha.1` becomes `0.1.0alpha.1`), and pacman sorts that before
  `0.1.0`. pkgrel is 1. pacman sorts some pre-releases differently from
  Semantic Versioning: `0.1.0-7` would become `0.1.07`, which is newer than
  `0.1.0`. So a pre-release must be a word and dot-separated numbers, such as
  `alpha.1` or `rc.2`. `release.py version` and `check-tag` refuse other
  pre-releases before anything is built.
* **Signature.** The package has no pacman signature. `SHA256SUMS` lists it,
  and on a tag run cosign and GPG sign `SHA256SUMS` and the file gets a
  build-provenance attestation, as for the other Linux packages. pacman
  installs a local package without a signature under Arch's default
  `LocalFileSigLevel = Optional`.
* **Name.** `persephone-<pkgver>-1-x86_64.pkg.tar.zst`. Dry runs and `main`
  builds add `-unsigned` before `.pkg.tar.zst`.
* **Guidance.** The release notes and the `latest` notes tell Linux users to
  use the package of their distribution and the AppImage only if none fits.

### Positive Consequences

* Arch Linux users install Persephone with pacman. pacman tracks its files
  and dependencies, and WebKitGTK and GTK get Arch's security updates.
* The programs are the same bytes as in the `.deb`, and `SHA256SUMS` lists
  both files.

### Negative Consequences

* The programs are built against Ubuntu 22.04 headers and run with Arch's
  newer libraries. The job checks that every library is present on the day
  of the build only. A later Arch update can break an installed package
  without a new Persephone build. `latest` is built again after every merge;
  a tagged release is not.
* The package has no pacman signature. Users must check `SHA256SUMS`.
* There is no Arch repository and no update. Users install each new file by
  hand. `latest` builds keep the version of the last release, so pacman
  reinstalls them instead of upgrading.
* There is no aarch64 package.
* Release versions have a narrower form: a pre-release of numbers only, such
  as `0.1.0-7`, is refused.
* The job trusts the pinned image and the current Arch repositories for
  makepkg, fakeroot, bsdtar and the check. Only makepkg's metadata files are
  added to the `.deb`'s files.

## Pros and Cons of the Options

### Repackage the x86_64 `.deb` with makepkg in an Arch Linux container

* Good, because the programs are those of the other Linux packages.
* Good, because the job is short and has no secrets.
* Bad, because the programs were not built against Arch's headers.

### Build from source in an Arch Linux container

* Good, because the programs are built against Arch's headers.
* Bad, because it is a second full build of the core, the workers and the
  call worker, with its own libwebrtc fetch, for each run.
* Bad, because the Arch programs then differ from the `.deb`'s, so a problem
  report from one package does not apply to the other.

### Publish a PKGBUILD in the AUR that downloads the released `.deb`

* Good, because AUR helpers can install and update it.
* Bad, because it is not a build artifact, needs an AUR account, and is a
  publication outside this repository. It can be added later on top of this
  package's PKGBUILD.

### Make no Arch package, and document the AppImage

* Good, because nothing changes.
* Bad, because Arch users keep a bundled WebKitGTK that pacman does not
  update, which the owner wants to avoid.

## Validation

26 November 2026, on the Linux Mint 22.3 test VM with Docker and
`archlinux:base-devel@sha256:3cbf69d2d5958d6a294f3722c5668e6187c4df8e00fa27f72352f0bf83e495e1`.
The input was `Persephone_0.3.0_amd64-unsigned.deb` from the `latest`
pre-release (commit `b45a06a`), checked against that release's
`SHA256SUMS`. The six `run` steps of the `arch-package` job, taken from
`release.yml`, ran in the container:

* makepkg 7.1.0 made `persephone-0.3.0-1-x86_64-unsigned.pkg.tar.zst`.
* The package's files were the same as the `.deb`'s.
* `pacman -U` installed it with its dependencies.
* Every library that the four programs link belonged to a package in
  `depends`, and `ldd` found every library.
* Negative control: with `libsoup3` removed from `depends`, the install step
  failed with "persephone-ui links libsoup-3.0.so.0 from libsoup3, which
  depends does not list".

Later the same day, after review, the job also compared each file's type,
mode, owner, group and link target, and `options` set each makepkg option
that changes files. The same VM, image and `.deb` ran the six steps again:

* All six steps passed. The `.deb`'s 27 entries were all owned by root.
* `pacman -U` installed the file under its published name,
  `persephone-0.3.0-1-x86_64-unsigned.pkg.tar.zst`.
* Negative controls: when `package()` changed the `.desktop` file's mode to
  0666, or its owner to 1:1, the file check failed and showed the changed
  entry. The `libsoup3` control failed as before.

Not done: a run of the job on GitHub Actions, and a start of the app on Arch
Linux with a display. A container has no display, session bus or Secret
Service.
