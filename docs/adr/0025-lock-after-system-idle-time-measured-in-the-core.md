# Lock after system idle time measured in the core

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for a lock-now shortcut
and a lock after a chosen idle time.

## Context and Problem Statement

Locking closes the protected session and needs the profile passphrase to
open again (ADR-0006, ADR-0008). Persephone locked only when the user
pressed the lock button. A computer left unattended with Persephone open
shows its rooms and can send as the account. Where is idle time measured,
and who enforces the lock?

## Decision Drivers

* The lock must happen even when Persephone's window is not in front.
* A compromised page must not be able to keep the profile open.
* No new dependency; explicit platform features only.

## Considered Options

* The page measures activity in its own window and calls `lock_session`.
* The core measures the computer's idle time and locks itself.

## Decision Outcome

Proposed option: "the core measures the computer's idle time".

* A setting "Lock Persephone when this computer is not used" (never, 5,
  15, 30 or 60 minutes; the core accepts 0 to 1440), off by default and
  kept in the encrypted profile like the other account settings.
* While a UI connection is served, a core thread checks every 15 seconds.
  When the setting is on, the profile is open, and the computer has had no
  keyboard or mouse input for that long, it locks through the same path as
  `lock_session`, so the page clears at once.
* On Windows the idle time is `GetLastInputInfo` against `GetTickCount`,
  taken with wrapping subtraction. This adds the `windows-sys` features
  `Win32_UI_Input_KeyboardAndMouse` and `Win32_System_SystemInformation`;
  no crate or version changes. Linux and macOS read no idle time yet: the
  setting is kept but never locks there, and an unknown idle time never
  locks.
* Ctrl+Shift+L (⌘⇧L on macOS) locks at once from anywhere in the window.

### Positive Consequences

* The lock does not depend on the page, nor on Persephone's window having
  focus: input in any program counts as activity, and its absence locks.

### Negative Consequences

* Input anywhere on the computer keeps Persephone open, which is weaker
  than counting only input in Persephone.
* A lock after the SDK client ran ends in `restart_required` (ADR-0008),
  so an idle lock may restart the core.
* No idle lock on Linux and macOS until their layers read idle time.
* Tested with unit tests of the decision only; the Windows calls and the
  thread are not exercised by an automated test.

## Links

* Uses the lock of [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md) and [ADR-0008](0008-restart-the-core-process-to-finish-a-lock-after-the-client-ran.md).
