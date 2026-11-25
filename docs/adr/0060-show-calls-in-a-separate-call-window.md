# Show calls in a separate call window

* Status: proposed
* Deciders: project owner
* Date: 2026-11-24

Technical Story: on 23 November 2026 the owner asked for a call window that
opens when a call starts. It shows a grid of every participant, with a
placeholder camera icon for anyone without video, and a participant list down
the right side that can be collapsed, with a level meter and a mute mark under
each name. The owner chose a separate operating system window. The owner also
asked for "Minimise to tray" at the bottom of the main menu, above "Exit".

## Context and Problem Statement

The call bar and the video tiles of ADR-0048 sit at the top of the main
window, above the room. A call with several cameras leaves little room for the
timeline, and a person without video has no tile. A second window is a second
web view. How does Persephone add it without giving that view more authority
than a call needs?

## Decision Drivers

* Keep the web view keyless (ADR-0004), with explicit commands per window.
* Keep the main window's navigation, new-window and capture rules for the
  new window.
* Keep the call bar in the main window, so the call can be controlled when
  the call window is closed.

## Considered Options

* A separate window with its own page and its own capability
* A panel inside the main window
* A second instance of the main page in a new window

## Decision Outcome

Chosen option: "A separate window with its own page and its own capability",
because it gives the owner the window they chose and the smallest set of
commands.

### Window

* The host creates the window `call` with the bundled `call.html` only. It
  uses the main window's navigation rule and refuses new windows.
* The capability `call` lets that window call `watch_session`,
  `call_status`, `call_video_frame`, `call_leave`, `call_mute`,
  `call_deafen`, `call_volume`, `call_camera`, `call_screen`,
  `close_call_window` and `call_window_ready`, and nothing else. It cannot read rooms, members,
  messages, account settings or files, start a call or start a device test.
* The main window opens it with `open_call_window` when a call starts to
  join, once for each call. The call bar has a "Call window" button that
  opens it again. It closes itself four seconds after the call ends. When the
  main window goes, the host closes it.
* On 25 November 2026 the owner asked to show the call's controls, video,
  volume, level meter and participants in the call window only. While the
  call window opens, the main window's call bar shows only the phone icon,
  the room, the phase, the "Call window" button, a "Leave" button and,
  after a declined ring (ADR-0052), the "Declined" note, which the call
  window does not show. It
  also shows the camera, screen share and device change notes, so a camera
  that did not start, a running or refused screen share and a failed
  microphone or speaker are seen while the call window is closed. The
  volume, the level meter and the participants show with the controls only. When the call window cannot open for
  the call, the call bar shows all of its controls and the main window shows
  the video tiles under it, so the call can still be muted or left. A later
  open that works makes the bar short again. An ended call still shows its
  reason and "Dismiss". This replaces the driver above for an open call
  window: the bar keeps the controls only when the call window cannot open,
  and its "Call window" button reopens a closed call window.
* Also on 25 November 2026 the owner asked for both the "Leave" button in
  the short bar and a ready signal. A shown window does not prove that its
  page works, so after its first `call_status` read the call page calls
  `call_window_ready`. The host accepts it from the `call` window only, and
  forgets it when a new call window is made or the call page loads again.
  `open_call_window` returns only when the page is ready. When the page
  does not say so within 10 seconds, the open fails with `unavailable`, and
  the main window shows the call's controls. At most four opens wait at
  once; another one fails at once. While an open waits, the "Call window"
  button reads "Opening the call window…" and takes no clicks. "Leave" in
  the short bar covers the wait, and a call page that stops working later.
* The host keeps each window's session, sound level and level channels under
  the window's label. Room subscriptions belong to the main window only. A
  call window that closes or reloads drops only its own channels.
* Capture protection (ADR-0045) applies to the call window as to the main
  window, through one applier for each window.

### Content

* The grid has a cell for you, one for each other device in the call and one
  for a shared screen. A cell shows its camera picture, or a placeholder
  camera icon. A speaking device has an outline.
* The participant list shows you and each device, each with a level meter
  (ADR-0059) and a "Microphone off" mark. A button hides and shows it.
* The window shows user IDs, not display names, because it has no access to
  the room's members. It uses the default theme, because it cannot read the
  account settings.

### Tray

* "Minimise to tray" (`minimise_to_tray`, main window only) hides both
  windows and shows a tray icon with the app's own icon and name. Its menu
  item "Show Persephone" brings the windows back and removes the icon. On
  Windows and macOS a left click does the same. Linux sends no icon clicks
  (Tauri 2.12.4), so there the menu is the way back. A call keeps running.
* Where no tray icon can be made, the command fails with `unavailable` and
  the windows stay shown. On Linux a built icon does not prove that a tray
  host shows it, so there the windows minimise instead of hiding and keep
  their taskbar entry. The app is never left with no window and no way back.
* The host registers the tray menu handler once, when the app starts, so a
  minimise and restore cycle does not add handlers.
* This needs Tauri's `tray-icon` feature. Its crate was already in the
  lockfile at the same version.

### Positive Consequences

* Calls with video no longer take space from the timeline.
* The call window has fewer commands than the main window.

### Negative Consequences

* A second web view uses more memory during a call.
* Names and the chosen theme are not shown in the call window.
* The page code for the grid repeats some of `CallVideo.svelte`.
* A call page that stops working after it said it is ready leaves the main
  window with "Leave" only, not mute. The "Call window" button only shows the
  same window, which still counts as ready, so it does not bring the full bar
  back. That needs the call window closed and opened again, or its page
  loaded again.
