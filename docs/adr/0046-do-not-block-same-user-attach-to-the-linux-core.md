# Do not block same-user attach to the Linux core

* Status: accepted
* Deciders: project owner
* Date: 2026-11-17

Technical Story: after the Windows code-loading and capture records
([ADR-0044](0044-report-code-loaded-into-persephone-and-ask-the-user-to-review-it.md),
[ADR-0045](0045-exclude-the-main-window-from-screen-capture-by-default.md)), the
owner asked whether the Linux core should block same-user attach.

## Context and Problem Statement

The core holds the keys (ADR-0004). On Linux, a process of the same user can
attach to the core with `ptrace`, or read `/proc/<pid>/mem`, unless the kernel
stops it. The Yama `ptrace_scope` setting decides this, and its value depends
on the distribution and the administrator. A process can stop such access to
itself with `prctl(PR_SET_DUMPABLE, 0)`. The media worker already does this
before it reads input (ADR-0010). The core does not.

The core is also the process that calls the desktop's OpenURI portal
(`org.freedesktop.portal.OpenURI`, `crates/core/src/platform/linux/portal.rs`).
Links in messages (ADR-0024) and OIDC sign-in through the browser (ADR-0030)
use that call. Should the Linux core make itself undumpable?

## Decision Drivers

* The owner decided on 17 November 2026 not to block attach on Linux if it is
  likely to cause problems.
* Link opening and OIDC sign-in must keep working on Linux.
* ADR-0024 opens links through the portal so that no shell or program is
  started with the address.
* A same-user attacker can also change the launcher, shell start-up files or
  autostart entries. Blocking attach raises the cost of an attack but is not
  a boundary against such an attacker.

## Considered Options

* Make the core undumpable at start-up
* Make the core undumpable and open links through a short-lived helper process
* Do not make the core undumpable

## Decision Outcome

Chosen option: "Do not make the core undumpable", because the owner chose
not to take a measure that is likely to break link opening and OIDC sign-in
through the portal.

* The core does not call `prctl(PR_SET_DUMPABLE, 0)`. Same-user attach to the
  core is controlled by the system's Yama setting.
* The media worker stays undumpable. It makes no portal calls.
* This record covers the Linux core only. It does not change the Windows
  mitigation policies or the macOS layer (ADR-0011).

### Measurement

A probe set `PR_SET_DUMPABLE` to 0 in a child process and read the child's
`/proc` files from its parent, as the same non-root user. The probe used
synthetic data only and is not committed.

* Environment: WSL 2, Ubuntu 24.04, kernel 6.6.87.2-microsoft-standard-WSL2,
  Yama `ptrace_scope` 1, Python 3.12.3, gnome-keyring 46.1-2ubuntu0.2.
  Run on 2026-11-17.
* Before the call, the parent could open `/proc/<pid>/mem`, `root` and
  `environ`. After the call, all three were refused with "Permission denied".
* `/proc/<pid>/stat`, `cgroup` and `status` stayed readable after the call.
* The undumpable child could still open a plain Secret Service session and
  search items through gnome-keyring.
* The portal was not installed in this environment. The portal refusal below
  comes from the upstream source and reports, not from this run.

### Portal behaviour

The upstream `xdg-desktop-portal` source (main branch, `shared/xdp-app-info*.c`,
read on 2026-11-17; the latest release in its `NEWS.md` was 1.22.0) identifies
each caller before it handles a call:

1. It opens `/proc/<pid>/root` and looks for `.flatpak-info`.
2. It reads `/proc/<pid>/cgroup` for a Snap.
3. Otherwise it treats the caller as a host app.

Only a "wrong app kind" result continues to the next step. If
`/proc/<pid>/root` cannot be opened, and the root is not on FUSE, the lookup
fails and the call is refused with "Unable to open /proc/<pid>/root".
Upstream issue [#1490](https://github.com/flatpak/xdg-desktop-portal/issues/1490)
reports the same refusal for KeePassXC and Bitwarden, which set the process
undumpable. Two pull requests that would have accepted such callers,
[#2119](https://github.com/flatpak/xdg-desktop-portal/pull/2119) and
[#2178](https://github.com/flatpak/xdg-desktop-portal/pull/2178), were closed
as insecure: a sandboxed app can also make itself undumpable, so a refused
`/proc/<pid>/root` does not prove that the caller is not sandboxed.

### Positive Consequences

* Link opening and OIDC sign-in through the portal keep working.
* Debuggers and crash tools keep working as the system allows.

### Negative Consequences

* On a system whose Yama setting allows it, another process of the same user
  can attach to the core and read its memory, including key material.
* The kernel may write a core dump of the core after a crash, as the system's
  core-dump settings allow. A dump can contain key material. Other ways to
  stop core dumps, such as `RLIMIT_CORE`, were not evaluated.
* A later change that adds `PR_SET_DUMPABLE` to the core breaks the portal
  calls. Such a change needs a new record that replaces this one.

## Pros and Cons of the Options

### Make the core undumpable at start-up

* Good, because it is one system call, set before any secret is loaded.
* Good, because it blocks same-user attach and `/proc/<pid>/mem` whatever the
  Yama setting is, and stops core dumps.
* Bad, because the portal refuses every call from an undumpable process. Link
  opening and OIDC sign-in would fail on Linux.
* Bad, because root, or a process with `CAP_SYS_PTRACE`, can still attach.

### Make the core undumpable and open links through a short-lived helper process

The core would start a helper for each confirmed link. A new program starts
dumpable, so the portal can identify the helper as a host app.

* Good, because the core stays undumpable for its whole life.
* Good, because the helper holds no keys and receives only a link the core
  has checked and the user confirmed.
* Bad, because it adds a process mode, its input checks and its tests to keep
  one portal call working.
* Bad, because each future portal use by the core needs the same treatment.

Turning the flag off around each portal call was not considered acceptable:
a waiting process can attach while the flag is off and stay attached after it
is set again.

### Do not make the core undumpable

* Good, because the portal calls keep working with no extra process.
* Bad, because protection against same-user attach depends on the system's
  Yama setting.

## Links

* Refines [ADR-0010](0010-linux-platform-layer.md)
* Related: [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md),
  [ADR-0024](0024-open-links-only-after-the-core-asks-in-its-own-window.md),
  [ADR-0030](0030-sign-in-with-oidc-through-the-browser-and-a-loopback-redirect.md),
  [ADR-0044](0044-report-code-loaded-into-persephone-and-ask-the-user-to-review-it.md)
