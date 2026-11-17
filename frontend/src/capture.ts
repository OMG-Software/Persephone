// Text for the screen capture card. Pure functions, no host calls.

import type { DisplaySession } from '../../crates/contracts/generated/contracts';

/**
 * Who can capture the window on Linux, by display server (ADR-0047). Linux
 * has no way for an app to hide its window, so this is information only.
 */
export function captureSessionText(session: DisplaySession | null): string {
  const base = 'Linux does not let apps hide a window from capture. ';
  if (session === 'x11') {
    return (
      base +
      'This window uses X11: any app on this desktop can read it and the whole screen ' +
      'without asking.'
    );
  }
  if (session === 'wayland') {
    return (
      base +
      'This window uses Wayland: apps capture the screen through your desktop, which usually ' +
      'asks you first. Some desktops let apps capture without asking.'
    );
  }
  return base + 'Persephone cannot tell whether this window uses Wayland or X11.';
}
