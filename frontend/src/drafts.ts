import type { RoomDraft } from '../../crates/contracts/generated/contracts';

export const EMPTY_DRAFT: RoomDraft = { body: '', replyTo: null };
export const MAX_DRAFT_BYTES = 16 * 1024;
export const DRAFT_SAVE_FAILED = 'The draft could not be saved. Keep this room open and try again, or discard the draft.';
export const DRAFT_LOAD_FAILED = 'The saved draft could not be loaded. Retry or explicitly discard it before composing a message.';
export const byteLength = (text: string) => new TextEncoder().encode(text).byteLength;

type Core = {
  load: (roomId: string) => Promise<RoomDraft>;
  save: (roomId: string, draft: RoomDraft) => Promise<void>;
};

/** Room-bound drafts. Writes are serialized; edits coalesce during the debounce. */
export class DraftKeeper {
  #core: Core;
  #show: (draft: RoomDraft, failure: string | null, ready: boolean) => void;
  #epoch = 0;
  #selection = 0;
  #revision = 0;
  #room: string | null = null;
  #draft: RoomDraft = EMPTY_DRAFT;
  #dirty = false;
  #loadFailed = false;
  #editing = false;
  #ready = false;
  #discardRequested = false;
  #timer: ReturnType<typeof setTimeout> | null = null;
  #saving: Promise<void> = Promise.resolve();

  constructor(core: Core, show: (draft: RoomDraft, failure: string | null, ready: boolean) => void) {
    this.#core = core;
    this.#show = show;
  }

  /** Invalidate pending reads and writes when the owning account session ends. */
  clear() {
    this.#epoch += 1;
    this.#selection += 1;
    this.#cancelTimer();
    this.#room = null;
    this.#draft = EMPTY_DRAFT;
    this.#dirty = false;
    this.#loadFailed = false;
    this.#editing = false;
    this.#ready = false;
    this.#discardRequested = false;
    this.#show(EMPTY_DRAFT, null, false);
  }

  /** Caller must flush before changing rooms. A late read cannot replace an edit. */
  async select(roomId: string) {
    this.#cancelTimer();
    const epoch = this.#epoch;
    const selection = ++this.#selection;
    this.#room = roomId;
    this.#draft = EMPTY_DRAFT;
    this.#dirty = false;
    this.#loadFailed = false;
    this.#editing = false;
    this.#ready = false;
    this.#discardRequested = false;
    const revision = ++this.#revision;
    this.#show(EMPTY_DRAFT, null, false);
    try {
      await this.#saving;
      if (epoch !== this.#epoch || selection !== this.#selection) return;
      const loaded = await this.#core.load(roomId);
      if (epoch !== this.#epoch || selection !== this.#selection || revision !== this.#revision) return;
      this.#draft = loaded;
      this.#ready = true;
      this.#show(loaded, null, true);
    } catch {
      if (epoch === this.#epoch && selection === this.#selection) {
        this.#loadFailed = true;
        this.#show(this.#draft, DRAFT_LOAD_FAILED, false);
      }
    }
  }

  /** Accept edits only after the selected room has loaded; oversized text stays unsaved. */
  update(draft: RoomDraft) {
    if (!this.#room || this.#editing || !this.#ready) return false;
    this.#revision += 1;
    this.#loadFailed = false;
    this.#draft = { ...draft };
    this.#dirty = true;
    this.#cancelTimer();
    if (byteLength(draft.body) > MAX_DRAFT_BYTES) {
      this.#show(this.#draft, 'The draft exceeds 16 KiB of UTF-8 text. Shorten it before saving or sending.', true);
      return false;
    }
    this.#timer = setTimeout(() => { void this.flush(); }, 350);
    return true;
  }

  /** Explicit user consent to replace the stored draft, including an unread one. */
  async discard() {
    if (!this.#room) return true;
    this.#editing = false;
    this.#revision += 1;
    this.#draft = EMPTY_DRAFT;
    this.#dirty = true;
    this.#discardRequested = true;
    return this.flush();
  }

  /** Editing an existing event must never become a new-message draft. */
  beginEdit() { this.#editing = true; }
  endEdit() { this.#editing = false; }

  /** Protecting keys takes priority over unsaved work. Do not wait for a
   * failed, oversized, or stalled draft write before lock or sign-out. The
   * core owns any admitted write until its lifecycle transition finishes. */
  protect<T>(action: () => Promise<T>): Promise<T> {
    this.#cancelTimer();
    return action();
  }

  /** false keeps a deliberate room/lock transition from losing unsaved text. */
  async flush(): Promise<boolean> {
    this.#cancelTimer();
    if (!this.#room || !this.#dirty) {
      await this.#saving;
      return !this.#dirty;
    }
    if (byteLength(this.#draft.body) > MAX_DRAFT_BYTES) return false;
    if (!this.#ready && !this.#discardRequested) return false;
    const epoch = this.#epoch;
    const selection = this.#selection;
    const revision = this.#revision;
    const roomId = this.#room;
    const draft = { ...this.#draft };
    const save = this.#saving.then(async () => {
      if (epoch !== this.#epoch) return false;
      try {
        await this.#core.save(roomId, draft);
        if (epoch !== this.#epoch) return false;
        if (selection === this.#selection && revision === this.#revision) {
          this.#dirty = false;
          this.#ready = true;
          this.#discardRequested = false;
          this.#show(this.#draft, null, true);
        }
        return true;
      } catch {
        if (epoch === this.#epoch && selection === this.#selection) this.#show(this.#draft, DRAFT_SAVE_FAILED, this.#ready);
        return false;
      }
    });
    this.#saving = save.then(() => {});
    const saved = await save;
    // More typing while a save was in flight must also finish before leaving.
    if (saved && epoch === this.#epoch && selection === this.#selection && this.#dirty) return this.flush();
    return saved;
  }

  /** Retry a failed load or save without replacing the current editor contents. */
  async retry() {
    if (this.#loadFailed && this.#room && !this.#dirty) {
      await this.select(this.#room);
      return !this.#loadFailed;
    }
    return this.flush();
  }

  /** Cancel the pending debounce so callers can flush or invalidate immediately. */
  #cancelTimer() {
    if (this.#timer !== null) clearTimeout(this.#timer);
    this.#timer = null;
  }
}
