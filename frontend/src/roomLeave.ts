/** A leave request must not make an unsaved room draft inaccessible. */
export async function leaveAfterDraftFlush(
  beforeleave: () => Promise<boolean>,
  leave: () => Promise<void>,
  current: () => boolean
): Promise<boolean> {
  if (!(await beforeleave()) || !current()) return false;
  await leave();
  return current();
}
