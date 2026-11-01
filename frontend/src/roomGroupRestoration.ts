export type SavedRoomGroup = { roomId: string; section: string; favourite: boolean };

/** Track saved values separately from unsaved controls and visible save errors. */
export class RoomGroupRestoration {
  #saved: SavedRoomGroup | null = null;

  /** Restore on a room or saved-value change, never on an unrelated list replacement. */
  changed(roomId: string, section: string, favourite: boolean): SavedRoomGroup | null {
    const previous = this.#saved;
    if (previous?.roomId === roomId && previous.section === section && previous.favourite === favourite) return null;
    const saved = { roomId, section, favourite };
    this.#saved = saved;
    return saved;
  }
}
