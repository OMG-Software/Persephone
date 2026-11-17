# Exclude the main window from screen capture by default

* Status: accepted
* Deciders: project owner
* Date: 2026-11-16

Technical Story: the owner asked for two further hardening steps after the
[injected DLL hardening](../evidence/injected-dll-hardening.md): an image-load
policy and protection of the window content against screen capture. The
image-load policy is recorded in that evidence. This record covers capture.

## Context and Problem Statement

Persephone shows decrypted messages in its main window. Any program running as
the same user can copy the screen or a window with the normal Windows capture
APIs, without injecting code. Examples are screen recorders, remote-support
tools and the Windows screenshot tools. How should Persephone treat capture of
its window?

## Decision Drivers

* The owner decided on 16 November 2026 to add capture protection, on by
  default, with a setting to turn it off.
* The UI process is keyless (ADR-0004). The window belongs to the UI process,
  and only the owning process can set its display affinity.
* Some users need capture: screen readers and magnifiers that read pixels,
  support sessions, and their own screenshots.
* Settings are stored per account in the core (`AccountSettings`).

## Considered Options

* Exclude the main window from capture, on by default, with a setting
* Exclude every window, including child and native dialog windows
* No capture protection

## Decision Outcome

Chosen option: "Exclude the main window from capture, on by default, with a
setting", because it is the owner's decision and the smallest change that
protects the message view.

* On Windows, the UI process calls `SetWindowDisplayAffinity` on its
  top-level main window with `WDA_EXCLUDEFROMCAPTURE`. If Windows refuses that
  value, it uses `WDA_MONITOR`, which shows the window as black in a capture.
  Off means `WDA_NONE`.
* Protection is on when the window is created, before any account setting is
  known.
* `capture_protection` is an account setting, `true` by default and for stored
  settings that do not have it. The UI host applies it from a successful
  `account_settings` or `set_account_settings` reply that it relays, and turns
  protection back on when the session changes. A reply to a call that started
  before the session changed is ignored. The window call runs on the window's
  thread, in the order of the changes.
* Settings > Security has the switch, with a short warning that capture tools
  and some accessibility tools then see the window as blank.
* Child windows and other processes' windows are not changed. The WebView2
  content is drawn into the protected top-level window.
* Other platforms do nothing. The setting has no effect there. On Linux,
  Settings says whether the window uses Wayland or X11
  ([ADR-0047](0047-report-loaded-code-on-linux-and-macos.md)).

### Positive Consequences

* Ordinary screen capture, recording and screen sharing do not show messages
  while the setting is on.
* The user can turn it off for accessibility or support.

### Negative Consequences

* This is not a boundary. A program running as the same user can read the
  process memory, inject code, or take a photograph of the screen. The display
  affinity only changes what the Windows capture APIs return.
* Pixel-based assistive tools, such as some magnifiers, can see a blank
  window. Screen readers that use UI Automation are not expected to be
  affected, because display affinity changes only captured pixels. This was
  not tested.
* A setting that the page sends passes through the keyless UI host. The UI host
  applies only what the core accepted and returned.
* Native file and credential dialogs are separate windows and are not
  protected.

## Pros and Cons of the Options

### Exclude the main window from capture, on by default, with a setting

* Good, because it covers the window that shows messages.
* Good, because it needs no window enumeration.
* Bad, because it can hide the window from tools the user wants.

### Exclude every window, including child and native dialog windows

* Good, because dialogs would also be protected.
* Bad, because display affinity applies only to top-level windows that the
  calling process owns. Child windows inherit the top-level window's
  protection, and native dialogs are created by system code at times the UI
  does not control.

### No capture protection

* Good, because it is simple and keeps every capture and accessibility tool
  working.
* Bad, because any capture tool can record decrypted messages.

## Links

* Refines [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Related: [ADR-0044](0044-report-code-loaded-into-persephone-and-ask-the-user-to-review-it.md)
* Related: [ADR-0047](0047-report-loaded-code-on-linux-and-macos.md)
* Evidence: [capture protection](../evidence/capture-protection.md)
