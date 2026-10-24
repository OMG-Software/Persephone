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

## Links

* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md),
  [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md),
  [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md),
  [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md)
* [ADR-0011](0011-macos-platform-layer.md) (macOS), [ADR-0012](0012-sign-and-publish-releases-as-drafts.md) (release)
