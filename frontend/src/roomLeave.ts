/** A leave request must not make an unsaved room draft inaccessible.
 * Check again after the flush because the selected room or session may have changed.
 */
export async function leaveAfterDraftFlush(
  beforeleave: () => Promise<boolean>,
  leave: () => Promise<void>,
  current: () => boolean
): Promise<boolean> {
  if (!(await beforeleave()) || !current()) return false;
  await leave();
  return current();
}

/** The room menu's question before it leaves. The join rule is not known
 * there, so the text covers both an invite-only and a public room. */
export const LEAVE_QUESTION =
  'Leave this room? You will stop receiving its messages. Unless anyone can join it, you will need a new invitation to come back.';

/** Shown in the composer, which is disabled while the leave is sent. */
export const LEAVING_TEXT = 'Leaving the room…';

/** Shown when the unsaved draft could not be kept, so the room was not left. */
export const LEAVE_NOT_DONE = 'The room was not left. Retry, or discard the unsaved draft first.';
