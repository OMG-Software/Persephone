// Texts for joining a room, from the public room directory or by accepting an
// invitation. The core waits up to five minutes for the homeserver
// (crates/core/src/joins.rs). A join that timed out, a busy homeserver and an
// unreachable one all reach the page as `core_unavailable`, and in each case
// the homeserver can still be joining.

/** A join that did not finish (`core_unavailable`). */
export const JOIN_UNFINISHED =
  'The homeserver did not finish joining the room. It can still be joining, because a big room on another '
  + 'server can take several minutes. If the join finishes, the room appears in the room list. If it does not '
  + 'appear, try again later.';

/** A join or an invitation answer while another one runs (`limit_exceeded`). */
export const JOIN_BUSY = 'Another room is being joined, or another invitation answered. Try again when that is done.';

/** Shown while a join runs. */
export const JOIN_WAIT = 'A big room on another server can take several minutes to join.';

/** Text for a failed answer to an invitation; `accept` is true for accepting. */
export function answerFailure(code: string, accept: boolean): string {
  switch (code) {
    case 'not_found':
      return 'That invitation is no longer pending.';
    case 'rejected':
      return accept
        ? 'The homeserver refused. The invitation may have been withdrawn, or you may be banned from the room.'
        : 'The homeserver refused to decline the invitation.';
    case 'limit_exceeded':
      return JOIN_BUSY;
    case 'locked':
      return 'The session is locked.';
    default:
      return accept ? JOIN_UNFINISHED : 'The homeserver could not be reached. Try again later.';
  }
}
