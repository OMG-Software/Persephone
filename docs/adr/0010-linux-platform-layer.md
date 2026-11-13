# Protect keys and contain processes on Linux

* Status: accepted
* Deciders: project owner
* Date: 2026-10-24

## Context and Problem Statement

Persephone must run on Linux. On Windows, DPAPI and the ADR-0006 passphrase
protect the store key, the core shows its own native windows for secrets
(ADR-0005), a Job caps the UI process (ADR-0004), and an AppContainer and Job
contain the media worker (ADR-0003). Linux has none of these. Persephone needs
Linux equivalents that keep the same boundaries and fail closed.

## Decision Drivers

* The store key must not be protected by the passphrase alone where the
  desktop offers a keyring.
* Secrets are typed into the core's own windows, never the page.
* The UI process must not be able to exhaust the memory of the system, and it
  must end when the core ends.
* The media worker must not reach the network, files or other processes, and
  the broker must check this before it sends input.
* Prefer mechanisms that need no root rights and no setuid helper.

## Considered Options

* Vault: Secret Service; the passphrase alone; a TPM-bound key.
* Prompts: GTK dialogs in the core; a separate prompt helper process.
* UI memory cap: a systemd user scope (cgroup v2); an address-space rlimit.
* Worker containment: a seccomp allowlist; Landlock and namespaces;
  bubblewrap.

## Decision Outcome

Chosen options (owner decisions, 24 October 2026, with the design approved in
the same plan):

* **Vault: Secret Service, else stop.** A random wrapping secret is kept in a
  Secret Service item, and the store key is wrapped with it and then sealed
  with the passphrase, as on Windows. Without a keyring, or with a locked
  collection that the user does not unlock, the profile stays locked and the
  app explains why. There is no passphrase-only fallback.
* **Prompts: GTK3 in the core**, on a dedicated thread, with the same texts,
  limits and defaults as the Windows windows. The file picker is a GTK native
  chooser; the core reads the chosen file with `O_NOFOLLOW` and the same size
  and length rules.
* **UI process:** the core starts it with a parent-death signal and
  `no_new_privs`, and places it in a systemd user scope with a memory limit
  equal to the Windows Job. **Without a systemd user manager, the UI starts
  anyway with a visible warning** that its memory is not capped.
* **Media worker: a seccomp allowlist.** The broker starts the worker with an
  empty environment, only its standard file descriptors, rlimits and
  `no_new_privs`. The worker installs a seccomp filter before it reads input:
  only reading and writing its two pipes, memory management and exit are
  allowed, and any other system call ends the process. The broker reads
  `/proc/<pid>/status` and sends input only when `NoNewPrivs` is 1 and
  `Seccomp` is 2.

### Implementation decisions (13 November 2026)

Owner decisions recorded with the implementation in
`crates/core/src/platform/linux/`:

* **Secret Service over a `plain` session.** The core talks to the Secret
  Service with `zbus` 5.19.0 (already in the lockfile through Tauri, now a
  direct, Linux-only dependency of the core) and opens a `plain` session, not
  the Diffie-Hellman one. The secret then crosses the session bus
  unencrypted. The session bus is a Unix socket that only the same user can
  reach, and that user can already ask the provider for the item from an
  unlocked collection, so the encrypted session would protect against no one
  who can read the bus; it would add key-agreement code to the core. The item
  is in the default collection, with the attributes `service`
  (`io.github.omg-software.persephone`), `account` (`store-key-wrap-v1`) and
  `xdg:schema`; its value is the 32-byte secret as 64 lowercase hexadecimal
  characters (`text/plain`), so providers that keep text keep it unchanged. It
  is never replaced: if two cores store at once, both use the item with the
  smallest object path. A locked collection is unlocked through the
  provider's own prompt, dismissed after 5 minutes without an answer. No
  session bus, no provider or no default collection is `unsupported`; a
  dismissed prompt or any other refusal leaves the key unreadable. Neither
  falls back to the kernel keyring, a file or the passphrase alone.
* **Prompts.** GTK starts on the `core-gtk` thread at the first prompt,
  without changing the process locale (`gtk_disable_setlocale`), and adds no
  chosen file to the recent-files list. Closing a yes-or-no question answers
  "No". Without a display every prompt and file chooser is refused.
* **Links.** A confirmed link is opened through the desktop's OpenURI portal
  (`org.freedesktop.portal.OpenURI`) as one D-Bus string. No shell,
  `xdg-open` or other program is started with the address. Without a portal
  the link is not opened.
* **UI process.** The parent-death signal is set in the child before `exec`.
  The kernel sends it when the *thread* that started the UI ends, so the core
  starts the UI from its main thread. The scope is a transient unit from
  `org.freedesktop.systemd1.Manager.StartTransientUnit` with `MemoryMax` of
  2 GiB (the Job's limit for the whole tree), `MemorySwapMax` of 0, so the UI
  cannot move past the cap into swap, and removal after it ends. The core
  waits until the kernel shows the UI in the scope and reads the cap back from
  the cgroup. A session without a systemd user manager, or a cgroup that does
  not show the cap (no memory controller for the user manager), gets the
  warning above. Any other refusal ends the UI. With a scope, the window watch
  counts the scope's processes, as the Job's on Windows. The Job's 1 GiB
  per-process limit has no equivalent: an address-space limit would stop the
  web view's large reservations.
* **Idle lock.** No idle time is read on Linux; the idle lock (ADR-0025)
  stays unavailable until the owner chooses its sources.
* **glib advisory.** RUSTSEC-2024-0429 (`glib::VariantStrIter`) was checked
  for the core's own GTK use: see
  [Dependency evidence](../evidence/dependencies.md#linux-platform-layer-gtk-and-zbus-in-the-core-13-november-2026).

### Positive Consequences

* The store key has the same two layers as on Windows.
* Unlike on Windows, the worker's network denial is enforced at the sender:
  a socket system call ends the process.
* No root rights, setuid helper or namespace support are needed.

### Negative Consequences

* Systems without a Secret Service provider (some window managers, servers)
  cannot open a profile.
* Without a systemd user manager, the UI's memory is not capped; the user is
  told.
* GTK runs inside the key-holding core. It is already in the dependency graph
  through Tauri, and the glib 0.18.5 advisory must be checked before any Linux
  build is distributed.
* The seccomp allowlist must be kept in step with the worker's actual system
  calls on each architecture (x86_64 and aarch64).
* The `plain` Secret Service session shows the wrapping secret to any process
  of the same user that monitors the session bus.
* Desktops without the OpenURI portal cannot open links from messages or the
  browser sign-in (ADR-0030).

## Links

* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md),
  [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md),
  [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md),
  [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md)
* [ADR-0011](0011-macos-platform-layer.md) (macOS), [ADR-0012](0012-sign-and-publish-releases-as-drafts.md) (release)
