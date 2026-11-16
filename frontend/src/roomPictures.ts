// Room pictures (ADR-0041). The core downloads a room's picture and decodes it
// in its contained worker; the page names only the room, never a URL. This
// keeps the decoded pixels of at most MAX_ROOM_PICTURES rooms, loads one
// picture at a time, and loads a picture only when asked: by the user, or for
// every listed room once the user turned on loading them automatically.
import type { RoomPicture } from '../../crates/contracts/generated/contracts';
import { BAD_IMAGE, assembleImage, validView, type Band } from './image.ts';

/** The core scales a room picture to fit this square. */
export const ROOM_PICTURE_SIDE = 96;
/** Most rooms whose pictures the page keeps: one per listed room (the core lists at most 200). */
export const MAX_ROOM_PICTURES = 200;
/** Most automatic tries of one picture that failed with a code worth trying again. */
export const MAX_AUTOMATIC_TRIES = 2;

/** How the page reaches the core: the bridge, or a test's fake. */
export type PictureCore = {
  load: (roomId: string) => Promise<RoomPicture>;
  band: (handle: string, index: number) => Promise<Band>;
  release: (handle: string) => Promise<void>;
};

export type PictureState =
  // Not loaded, and not asked for.
  | { kind: 'none' }
  | { kind: 'loading' }
  | { kind: 'shown'; width: number; height: number; pixels: Uint8ClampedArray<ArrayBuffer> }
  | { kind: 'failed'; reason: string; retry: boolean };

type Entry = {
  pictureId: string;
  state: PictureState | { kind: 'queued' };
  // The user asked for it; otherwise it was loaded automatically.
  asked: boolean;
  tries: number;
};

/** A listed room and the ID of its current picture, if it has one. */
export type ListedRoom = { roomId: string; pictureId: string | null };

const NONE: PictureState = { kind: 'none' };
const LOADING: PictureState = { kind: 'loading' };

/** True for the ID the core gives a picture: 16 lowercase hex characters. */
export const validPictureId = (id: unknown): id is string => typeof id === 'string' && /^[0-9a-f]{16}$/.test(id);

/** The core gives the same answer again for these. */
const final = (reason: string) => reason === 'rejected' || reason === 'not_found' || reason === BAD_IMAGE;

export class RoomPictures {
  readonly limit: number;
  #core: PictureCore;
  #entries = new Map<string, Entry>();
  #listeners = new Set<() => void>();
  #running = false;
  // Advanced by clear(): a load from before it keeps nothing.
  #generation = 0;

  constructor(core: PictureCore, limit: number = MAX_ROOM_PICTURES) {
    this.#core = core;
    this.limit = limit;
  }

  /** The picture of `roomId` when it is the one named `pictureId`. */
  state(roomId: string, pictureId: string | null): PictureState {
    const entry = this.#entries.get(roomId);
    if (!entry || pictureId === null || entry.pictureId !== pictureId) return NONE;
    return entry.state.kind === 'queued' ? LOADING : entry.state;
  }

  /** Calls `listener` after every change; returns the function that stops it. */
  subscribe(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  get size(): number {
    return this.#entries.size;
  }

  /**
   * The user asks for the picture `pictureId` of `roomId`. A picture already
   * shown or loading is not asked for again; one that failed is, unless the
   * core would give the same answer.
   */
  request(roomId: string, pictureId: string): void {
    this.#queue(roomId, pictureId, true);
  }

  /**
   * The rooms now listed. Pictures of rooms no longer listed, or whose
   * picture changed or was removed, are dropped. With `automatic`, every
   * listed picture not yet loaded is queued; without it, pictures queued
   * automatically and not yet started are dropped.
   */
  sync(rooms: readonly ListedRoom[], automatic: boolean): void {
    const listed = new Map(rooms.map((room) => [room.roomId, room.pictureId]));
    let changed = false;
    for (const [roomId, entry] of this.#entries) {
      const pictureId = listed.get(roomId);
      const stale = pictureId === undefined || pictureId === null || pictureId !== entry.pictureId;
      const unwanted = !automatic && !entry.asked && entry.state.kind === 'queued';
      if (stale || unwanted) {
        this.#entries.delete(roomId);
        changed = true;
      }
    }
    if (changed) this.#notify();
    if (!automatic) return;
    for (const room of rooms.slice(0, this.limit)) {
      if (room.pictureId !== null) this.#queue(room.roomId, room.pictureId, false);
    }
  }

  /** Drops every picture, and any load in progress keeps nothing: the session changed. */
  clear(): void {
    this.#generation += 1;
    this.#entries.clear();
    this.#running = false;
    this.#notify();
  }

  #queue(roomId: string, pictureId: string, asked: boolean) {
    if (!validPictureId(pictureId)) return;
    const entry = this.#entries.get(roomId);
    if (entry && entry.pictureId === pictureId) {
      const { state } = entry;
      if (state.kind !== 'failed') return;
      if (asked ? !state.retry : final(state.reason) || entry.tries >= MAX_AUTOMATIC_TRIES) return;
      entry.state = { kind: 'queued' };
      entry.asked ||= asked;
    } else {
      this.#entries.delete(roomId);
      this.#entries.set(roomId, { pictureId, state: { kind: 'queued' }, asked, tries: 0 });
      this.#evict();
    }
    this.#notify();
    void this.#run();
  }

  // Drops the oldest pictures that are not loading while there are too many.
  #evict() {
    for (const [roomId, entry] of this.#entries) {
      if (this.#entries.size <= this.limit) return;
      if (entry.state.kind !== 'loading') this.#entries.delete(roomId);
    }
  }

  #next(): [string, Entry] | null {
    for (const pair of this.#entries) if (pair[1].state.kind === 'queued') return pair;
    return null;
  }

  async #run() {
    if (this.#running) return;
    this.#running = true;
    const generation = this.#generation;
    try {
      for (let next = this.#next(); next !== null && generation === this.#generation; next = this.#next()) {
        const [roomId, entry] = next;
        entry.state = LOADING;
        entry.tries += 1;
        this.#notify();
        const result = await this.#load(roomId, entry.pictureId, generation);
        if (generation !== this.#generation) return;
        // The room's picture may have changed meanwhile; then nothing is kept.
        if (this.#entries.get(roomId) === entry) {
          entry.state = result;
          this.#notify();
        }
      }
    } finally {
      if (generation === this.#generation) this.#running = false;
    }
  }

  async #load(roomId: string, pictureId: string, generation: number): Promise<PictureState> {
    let handle: string | null = null;
    try {
      const reply = await this.#core.load(roomId);
      handle = reply.image.handle;
      // Another picture than the one asked for: it changed in the core first.
      // The room list names the new one soon, and that is loaded then.
      if (reply.pictureId !== pictureId) return { kind: 'failed', reason: 'changed', retry: true };
      const { image } = reply;
      if (!validView(image) || image.width > ROOM_PICTURE_SIDE || image.height > ROOM_PICTURE_SIDE) throw BAD_IMAGE;
      const pixels = await assembleImage(
        image,
        (index) => this.#core.band(image.handle, index),
        () => generation !== this.#generation
      );
      if (pixels === null) return NONE;
      return { kind: 'shown', width: image.width, height: image.height, pixels };
    } catch (reason) {
      const code = String(reason);
      return { kind: 'failed', reason: code, retry: !final(code) };
    } finally {
      // The pixels live in the page from now on, or nowhere.
      if (handle !== null) this.#core.release(handle).catch(() => {});
    }
  }

  #notify() {
    for (const listener of this.#listeners) listener();
  }
}

// Error codes map to fixed, short texts; nothing else is shown.
const failureText: Record<string, string> = {
  rejected: 'This room picture cannot be shown: the server refused it, or it is damaged or not a PNG or JPEG.',
  not_found: 'This room has no picture now.',
  limit_exceeded: 'Another picture is loading. Try again when it finishes.',
  locked: 'The session is locked.',
  changed: 'The room picture changed while it loaded. Try again.',
  [BAD_IMAGE]: 'The picture data was not valid, so it is not shown.'
};

/** The text for a picture that failed to load. */
export function pictureFailureText(reason: string): string {
  return failureText[reason] ?? 'The room picture could not be loaded right now. Try again.';
}
