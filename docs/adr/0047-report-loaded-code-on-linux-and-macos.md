# Report loaded code on Linux and macOS

* Status: accepted
* Deciders: project owner
* Date: 2026-11-17

Technical Story: [ADR-0044](0044-report-code-loaded-into-persephone-and-ask-the-user-to-review-it.md)
reports the code loaded into Persephone on Windows only.
[ADR-0046](0046-do-not-block-same-user-attach-to-the-linux-core.md) keeps the
Linux core attachable by the same user. The owner asked for the same report on
Linux and macOS, with a warning for new code.

## Context and Problem Statement

Linux and macOS load code into a process in other ways than Windows. Linux has
`LD_PRELOAD`, `LD_AUDIT` and `/etc/ld.so.preload`, and no signatures for
libraries. macOS signs code, and the hardened runtime refuses most injected
libraries, but a process cannot read another process's images without
debugging rights. Neither system lets an app hide its window from capture. How
does Persephone report loaded code on these systems, and what does the user
learn about capture?

## Decision Drivers

* The owner chose on 17 November 2026:
  * a Linux report of the core, UI and web view processes;
  * a Linux check for fileless executable memory, in the core only;
  * a macOS report of the core only, marked incomplete;
  * a Linux capture note that says whether the window uses Wayland or X11.
* The owner chose the Linux rule "system and own bundle": hide system files
  that only root can change and files of the read-only image Persephone runs
  from; show everything else and every preloaded library.
* The same review flow, notice and limits as ADR-0044. Windows review ids must
  stay valid.
* No network access, no logged paths, and no new dependency.

## Considered Options

* Judge Linux files by who can change them
* Judge Linux files by package-manager ownership
* Report every Linux file that is not in a system folder

## Decision Outcome

Chosen option: "Judge Linux files by who can change them", because it is the
owner's rule, it works on every distribution and with AppImage, and it needs
no package database.

### Contract

* `ModuleSignature` gains `not_checked` for Linux, which has no signatures.
* Each row gains a `reason`: `signer` (Windows, macOS), `preloaded`,
  `no_file`, `user_writable` or `outside_system` (Linux).
* A Linux review id is the lowercase file name and the reason. A Windows id is
  unchanged, so saved reviews stay valid. A file listed for a new reason shows
  as new again.
* A file listed in several processes shows its most serious reason:
  `preloaded`, then `no_file`, `user_writable` and `outside_system`.

### Linux

* The core reads `/proc/<pid>/maps` and `/proc/<pid>/environ` of the core, the
  UI and the processes in the UI's cgroup or process group. It uses the same
  30 s schedule as Windows.
* These files are expected and not reported:
  * Persephone's own executables in the core's folder;
  * any file on the read-only file system of the core's executable (an
    AppImage);
  * a file in `/usr`, `/lib`, `/lib32`, `/lib64`, `/libx32`, `/nix/store` or
    `/gnu/store` that root owns and that group and others cannot write, in
    folders that meet the same rule. A sticky folder counts when root owns
    the entry, so `/nix/store` works.
* These are reported, most serious first:
  * a file named in `LD_PRELOAD`, `LD_AUDIT` or `/etc/ld.so.preload`, even a
    system file or a file of Persephone's own image. In
    `/etc/ld.so.preload`, a `#` starts a comment to the end of the line, as in
    glibc's loader;
  * in the core only, executable memory with no file: anonymous memory,
    `memfd` and named regions such as `[heap]`. The kernel's `[vdso]`,
    `[vsyscall]` and `[uprobes]` pages are not code from a file and are
    skipped. The web view's JIT makes such memory normal, so other processes
    are not checked for it;
  * a file deleted since it was loaded, unless the file now at its path is
    expected (a package update);
  * a file that a user other than root can change, or in such a folder;
  * any other file.
* A process in another mount namespace is not judged, because its paths name
  other files. The scan is then incomplete. So are a cut `maps` or `environ`
  read, a cut `cgroup.procs` list, more than 8192 files in one process, and an
  unreadable `/etc/ld.so.preload`.
* Paths are compared as they are. Two Linux paths that differ only in case
  are two files and two rows.
* Reads are bounded: 8 MiB of `maps`, 1 MiB of `environ`, 64 KiB of
  `/etc/ld.so.preload`, and at most 64 folders above one file.

### macOS

* The core lists its own images through dyld. It cannot read the UI and web
  view processes, so every report is incomplete.
* These images are expected and not reported: images in the dyld shared
  cache; Persephone's own executables in the core's folder; Apple's code (the
  requirement `anchor apple`); and code with a valid signature inside the
  `.app` bundle the core runs from.
* Every other image is reported with its signature state and, when the
  signature is valid, the common name of the leaf certificate.
* Signature checks use `kSecCSNoNetworkAccess`: no revocation or other online
  check is fetched.
* A signature check reads the whole file, so its result is kept while the
  file's device, inode, size and times are unchanged.

### Capture note on Linux

* The UI host answers `display_session` with `wayland`, `x11` or `unknown`.
  At start, on the GTK main thread, it reads the type of the GDK display the
  window opened on (`GdkWaylandDisplay` or `GdkX11Display`). GTK names the
  backend that actually opened, so a Wayland session where GTK fell back to
  X11 reads as X11. The core is not asked: it has no window.
* Settings > Security says that Linux cannot hide a window from capture and
  which display server draws the window. It is information only.

### Positive Consequences

* Linux users see preloaded and fileless code in the core, which is how most
  same-user injection on Linux works without a debugger.
* AppImage, distribution packages and Nix stores work without a package
  database.
* macOS users see any unexpected image in the core, such as a debug build's
  libraries.

### Negative Consequences

* This is transparency, not a boundary, as in ADR-0044. Same-user code can
  attach to the core (ADR-0046), write its memory, and hide from this report.
* The Linux rule trusts root. A file that root installed in a system folder
  is never reported, even if it is unwanted.
* A Linux `.deb` or `.rpm` install that puts Persephone outside a system
  folder shows its own libraries as `outside_system`. The user can review
  them once.
* The fileless check has not been measured on every desktop. A GTK theme,
  Mesa or libffi in the core can make executable anonymous memory and show as
  `no_file`.
* The macOS report cannot see the UI and web view processes. The hardened
  runtime's library validation protects them; this report does not.
* The UI host links `gtk` directly to read the display type. It is the
  version Tauri already links, so no new native library is added.
* macOS is built and tested on CI only.

## Pros and Cons of the Options

### Judge Linux files by who can change them

* Good, because it works on every distribution, AppImage and Nix.
* Good, because a file the user can change is the file an attacker with the
  user's rights can change.
* Bad, because it trusts every file root installed.

### Judge Linux files by package-manager ownership

* Good, because it names files that no package installed.
* Bad, because there are many package databases, and reading them is slow
  and needs parsers.

### Report every Linux file that is not in a system folder

* Good, because it is simple.
* Bad, because a library in a system folder that a user can change is not
  reported, and every AppImage library is.

## Links

* Refines [ADR-0044](0044-report-code-loaded-into-persephone-and-ask-the-user-to-review-it.md)
* Related: [ADR-0045](0045-exclude-the-main-window-from-screen-capture-by-default.md)
* Related: [ADR-0046](0046-do-not-block-same-user-attach-to-the-linux-core.md)
* Refines [ADR-0010](0010-linux-platform-layer.md) and
  [ADR-0011](0011-macos-platform-layer.md)
