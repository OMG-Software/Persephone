// Full screen for one element of the call window (ADR-0060). It uses the
// web view's DOM Fullscreen API: no host command is needed, and the web
// view turns its window full screen with the element. These functions take
// the document as an argument, so a test can pass a fake.

/** The part of `Document` that these functions use. */
export interface FullscreenDocument {
  readonly fullscreenElement: unknown;
  exitFullscreen(): Promise<void>;
}

/** The part of `Element` that these functions use. */
export interface FullscreenTarget {
  requestFullscreen(): Promise<void>;
}

/** True when `element` is the element that fills the screen now. */
export function isFullscreen(doc: Pick<FullscreenDocument, 'fullscreenElement'>, element: unknown): boolean {
  return element !== null && element !== undefined && doc.fullscreenElement === element;
}

/**
 * Tells the host whether the document has a full screen element now. A
 * failed send is dropped: the window stays as it was.
 */
export async function reportFullscreen(
  doc: Pick<FullscreenDocument, 'fullscreenElement'>,
  send: (on: boolean) => Promise<void>
): Promise<void> {
  try {
    await send(doc.fullscreenElement !== null && doc.fullscreenElement !== undefined);
  } catch {
    // The window stays as it was.
  }
}

/**
 * Puts `element` in full screen, or takes it out when it is there already.
 * A refusal (no user gesture, no support, a window that cannot change) leaves
 * the window as it was and shows nothing new, so it is dropped.
 */
export async function toggleFullscreen(doc: FullscreenDocument, element: FullscreenTarget): Promise<void> {
  try {
    if (isFullscreen(doc, element)) await doc.exitFullscreen();
    else await element.requestFullscreen();
  } catch {
    // The window stays as it was.
  }
}
