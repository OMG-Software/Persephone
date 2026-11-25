# Choose the large video and change devices in the call window

* Status: proposed
* Deciders: project owner
* Date: 2026-11-25

Technical Story: the call window of ADR-0060 shows every feed at the same
size. A viewer who wants to read a shared screen, or to watch one person, has
no way to enlarge it. A viewer who plugs in a headset must leave the call
window and use the main window's settings.

## Context and Problem Statement

The call window shows a grid with one cell for you, one for each other device
and one for a shared screen. The worker sends one picture for each tile at a
fixed size: at most 640 by 360 pixels for a camera and 1280 by 720 for the
screen (ADR-0048, ADR-0053). The window may call only the commands in its
capability (ADR-0060). How does the viewer choose a large feed and change
devices, without asking the core for larger pictures and without giving the
call window more commands than it needs?

## Decision Drivers

* Keep the pictures and the worker as they are: the page scales with CSS.
* Keep the call window's commands to the fewest that the feature needs.
* Keep the choice logic testable without a web view.
* Keep the controls usable by keyboard and by screen reader.

## Considered Options

* Selectable feeds with a large view and a row of small tiles; a device panel
  inside the call window
* A fixed layout that always enlarges the screen share; devices only in the
  main window
* Ask the core for a larger picture for the large feed

## Decision Outcome

Chosen option: "Selectable feeds with a large view and a row of small tiles;
a device panel inside the call window", because it needs no change to the
worker or the core and adds two read and write commands that the main window
already has.

### Large video

* Each feed (your camera, each other camera, the shared screen) is one
  button with `aria-pressed` and a visible focus ring.
* With no pick the window shows the grid of ADR-0060. Selecting a feed shows
  it large. The other feeds become small tiles in one row along the bottom;
  the row scrolls sideways when the tiles do not fit. Selecting another small
  feed makes it large and puts the one that was large back in the row.
  Selecting the large feed again, or "Show all", shows the grid.
* When a screen is shared and the viewer has picked nothing, the screen is
  large. A pick by the viewer always wins, including "Show all": the grid
  stays while that share runs, and the next share is shown large again.
* A feed that goes away (the person leaves, the camera goes off, the share
  ends) drops its pick. The window then shows the grid, or the screen by the
  rule above. The pick is also dropped when the call ends.
* The rules are pure functions in `frontend/src/callView.ts`, with Node tests.
* The picture sizes do not change. A large feed is the same picture scaled
  by CSS with its aspect ratio kept, so a camera is enlarged beyond its
  640 by 360 pixels and looks soft. A feed that moves between the grid, the
  large view and the row gets a new canvas; the page draws the newest picture
  again at once from a copy it keeps for each tile. The copy is kept with the
  feed (the device and stream) it belongs to, and is drawn only while that
  feed is still on the tile, so a tile that another person takes next never
  shows the last picture of the one before.

### Devices in the call window

* A "Device settings" button in the call window's controls opens a panel with
  the microphone, speaker and camera. The panel lists the devices each time it
  opens. The button, or Escape, closes it and the focus returns to the button.
* A choice is saved with `set_call_devices` and applies to the running call
  through the switch of ADR-0059. The core checks the IDs again.
* Saves go one at a time, in the order the menus changed, and each sends all
  three menus as they were then. A menu changed while a save runs is
  therefore sent after it and never dropped, and only the newest save sets
  the line under the menus. The menus stay usable while a save runs; they
  wait only while the lists load. The order rules are in
  `frontend/src/callDevicePanel.ts`, with Node tests.
* The capability `call` gets two more commands: `call_devices` and
  `set_call_devices`. It still has no device tests, no call start, no rooms
  and no files. The host test of the call window's command set checks this.
* The panel shows what happened: saved; the chosen device is not connected
  (`not_found`); the microphone could not be opened and the old one is kept
  (`kept`); this system cannot switch during a call (`unsupported`, Linux
  today); or the sound may have stopped and a lost speaker needs a rejoin
  (`failed`). A call that ends because of its audio device shows its end
  reason as before (`AudioDevice`).
* The call status has no mark for a change that worked. The panel therefore
  says only "saved" when it saved the choice and the status shows no
  failure, and adds that a failure shows on the same line. It cannot tell
  "worked" from "not answered yet", so it never says "switched".
  A failure that the status shows is shown whether or not the panel saved
  anything, so a failure from before the panel opened (or from the main
  window's settings) is in view when the panel opens to fix it.
  The worker's answer can arrive after the save, and a failure from an earlier
  change stays until the worker answers the new one. A camera change applies
  at the next camera start (ADR-0059).

### Positive Consequences

* A viewer can read a shared screen or watch one person at a larger size.
* Devices change without leaving the call window.

### Negative Consequences

* Two more commands in the call window's capability.
* A camera picture is not sharper when it is large.
* "Switched" is a reading of the absence of a failure, not an
  acknowledgement. A later change to the status can add one.
* The page keeps the newest picture of each tile, up to about 8 MB.
