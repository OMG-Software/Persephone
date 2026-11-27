// Full screen for one element of the call window (ADR-0060). It uses the
// web view's DOM Fullscreen API. On macOS and Linux the web view turns its
// window full screen with the element. On Windows it only fills the web
// view, so the page reports each change to the host (`FullscreenReporter`),
// which turns the call window full screen. The reporter keeps one report in
// flight, so reports cannot reach the host in another order. These functions
// take the document as an argument, so a test can pass a fake.
import { LatestOnly } from './calls.ts';

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
 * Tells the host whether the document has a full screen element. One report
 * is in flight at a time. While it waits, only the newest state is kept and
 * it goes next, so the last state sent is the state of the last change. A
 * failed send is dropped: the window stays as it was, and the next report
 * still goes.
 */
export class FullscreenReporter {
  #doc: Pick<FullscreenDocument, 'fullscreenElement'>;
  #only: LatestOnly<boolean>;

  constructor(doc: Pick<FullscreenDocument, 'fullscreenElement'>, send: (on: boolean) => Promise<void>) {
    this.#doc = doc;
    this.#only = new LatestOnly<boolean>(send);
  }

  /** Reports the state of the document now. */
  report(): void {
    this.#only.set(this.#doc.fullscreenElement !== null && this.#doc.fullscreenElement !== undefined);
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
