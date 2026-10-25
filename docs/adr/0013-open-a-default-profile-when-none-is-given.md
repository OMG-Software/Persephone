# Open a default profile when none is given

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-25

Technical Story: the owner started a locally built release `persephone.exe`
on 25 October 2026 and saw the synthetic rooms instead of a sign-in.

## Context and Problem Statement

The core opened a profile only with `--profile <dir>`. Without it, the core
served synthetic rooms and stored nothing. Installed builds start the core
without arguments: the shortcut starts `persephone-ui`, which starts
`persephone` next to it with no arguments (`src-tauri/src/launcher.rs`).
Every installed copy therefore showed the synthetic rooms and could not sign
in.

Where must the core keep the profile when no directory is given, and how is
the synthetic mode reached?

## Decision Drivers

* The first release has one account and must sign in to a real homeserver
  from a normal start.
* The UI is keyless and cannot choose where the core stores data (ADR-0004).
  The launcher passes no arguments on purpose.
* The stores hold this device's Matrix keys. Two computers must never run
  the same device from copies of one profile.
* Installer upgrades and uninstalls must not change the profile.
* Fail closed: an unknown location must not fall back to the working
  directory or to synthetic data.
* No new crate in the key-holding core.

## Considered Options

* **A. A fixed per-user local folder, chosen by the core from the OS.**
* **B. Pass `--profile` from the installer's shortcut.**
* **C. Keep the profile next to the executable.**
* **D. Use Tauri's application data folder (named after the identifier).**

## Decision Outcome

Chosen option: "A", because it works for every way the app is started, the
UI has no part in it, and it keeps the profile apart from installed files.

| OS | Default profile |
| --- | --- |
| Windows | `%LOCALAPPDATA%\OMG-Software\Persephone\profile`, from `SHGetKnownFolderPath(FOLDERID_LocalAppData)` |
| macOS | `~/Library/Application Support/OMG-Software/Persephone/profile`, from `HOME` |
| Linux | `$XDG_DATA_HOME/persephone/profile`, else `$HOME/.local/share/persephone/profile` |

* `--profile <dir>` still opens that directory instead.
* `--demo` serves the synthetic rooms and stores nothing. It cannot be
  combined with `--profile`. `--demo-live` needs `--demo`. These options stay
  in release builds; the rooms contain no secrets and help to check the UI.
* Windows uses the local, not the roaming, application data folder.
* On Unix a relative or empty `XDG_DATA_HOME` is ignored, as the XDG Base
  Directory rules require. A missing or relative `HOME` gives no default.
* With no default, the window starts in `store_unavailable`. Unlock and Reset
  do nothing, and the views stay locked.
* New profile directories, and the parents created for them, get mode 0700 on
  Unix. Existing directories keep their mode. Windows directories inherit the
  ACL of the user's local application data folder.
* Linux still has no vault (ADR-0010), so its default profile stays
  `unsupported` until that layer exists.

### Positive Consequences

* Installed and directly started builds open a real profile. On Windows and
  macOS they can then sign in; Linux cannot until its vault and UI host exist
  (ADR-0010).
* The profile is not in the per-user install directory
  (`%LOCALAPPDATA%\Persephone` for the NSIS installer) or the web view's data
  folder (`%LOCALAPPDATA%\io.github.omg-software.persephone`).
* A roaming Windows profile does not copy the stores to other computers.
* Synthetic rooms appear only when asked for.

### Negative Consequences

* One default profile per user account. A second running copy gets
  `profile_in_use` from the existing `profile.lock`; it does not bring the
  first window forward.
* A profile already used with `--profile` is not moved. Its owner keeps
  passing `--profile`, or moves the folder while Persephone is closed.
* Development runs that relied on the synthetic rooms without arguments now
  need `--demo`.
* `windows-sys` gains the `Win32_UI_Shell` feature in the core.

## Pros and Cons of the Options

### A. A fixed per-user local folder, chosen by the core

* Good, because no caller, shortcut or UI decides the location.
* Good, because the folder survives upgrades and is never roamed.
* Bad, because the vendor and product names are fixed in code; a rename needs
  a migration.

### B. Pass `--profile` from the installer's shortcut

* Good, because the core stays unchanged.
* Bad, because the launcher passes no arguments, and each installer format
  (NSIS, MSI, `.deb`, `.rpm`, AppImage, `.app`) would need its own shortcut
  arguments.
* Bad, because starting the executable directly would still show synthetic
  rooms.

### C. Keep the profile next to the executable

* Good, because it is simple to find.
* Bad, because the installer writes and deletes there, and a per-machine
  install is not writable by the user.

### D. Use Tauri's application data folder

* Good, because Tauri already resolves it on each OS.
* Bad, because the web view keeps its data there, and Tauri's NSIS
  uninstaller offers to delete the application data, which would take the
  keys with it. This was not checked against the bundler's template.
* Bad, because the key-holding core would depend on a UI framework path.

## Links

* Refines [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Relates to [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md)
* Relates to [ADR-0010](0010-linux-platform-layer.md) and [ADR-0011](0011-macos-platform-layer.md)
