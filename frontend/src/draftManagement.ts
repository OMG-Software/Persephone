import type { RoomDraft } from '../../crates/contracts/generated/contracts';

/** The active editor owns its draft. The manager can remove only inactive ones. */
export async function discardInactiveDraft(
  roomId: string,
  selected: () => string | null,
  save: (roomId: string, draft: RoomDraft) => Promise<void>
): Promise<boolean> {
  if (selected() === roomId) return false;
  await save(roomId, { body: '', replyTo: null });
  return true;
}
