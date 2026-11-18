// Call video (ADR-0048). The core decodes and scales every picture in its
// contained call worker; the page gets plain RGBA pixels, one picture at a
// time, and draws them on a canvas. Nothing here decodes video. The raw
// response is checked again here, because a canvas is drawn only from a
// complete, consistent buffer.

/** The raw `call_video_frame` prefix: seq u64, rotation, width, height as u32, all little-endian. */
export const VIDEO_PREFIX_BYTES = 20;
/** The longest side of a picture, in pixels. */
export const MAX_VIDEO_SIDE = 640;
/** The most pixels in one picture. */
export const MAX_VIDEO_PIXELS = 640 * 360;
/** Video tiles in a call: tile 0 is the own camera, tiles 1 to 4 are remote. */
export const VIDEO_TILES = 5;
/** The largest rotation: quarter turns clockwise. */
export const MAX_VIDEO_ROTATION = 3;

/** The error a picture outside the contract gives. */
export const BAD_VIDEO = 'bad_video';

export type VideoPicture = {
  seq: number;
  rotation: number;
  width: number;
  height: number;
  pixels: Uint8ClampedArray<ArrayBuffer>;
};

const isSide = (value: number) => Number.isInteger(value) && value >= 1 && value <= MAX_VIDEO_SIDE;

/**
 * Reads one raw `call_video_frame` response. An empty body means no newer
 * picture and gives null. Anything else must be exactly the prefix and
 * `width * height * 4` pixels inside the limits, with a rotation of 0 to 3 and
 * a picture number JavaScript holds exactly; otherwise this throws `BAD_VIDEO`.
 */
export function parseVideoPicture(raw: unknown): VideoPicture | null {
  let bytes: Uint8Array;
  if (raw instanceof ArrayBuffer) {
    bytes = new Uint8Array(raw);
  } else if (ArrayBuffer.isView(raw)) {
    bytes = new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength);
  } else {
    throw BAD_VIDEO;
  }
  if (bytes.byteLength === 0) return null;
  if (bytes.byteLength < VIDEO_PREFIX_BYTES) throw BAD_VIDEO;
  const prefix = new DataView(bytes.buffer, bytes.byteOffset, VIDEO_PREFIX_BYTES);
  const seq = prefix.getBigUint64(0, true);
  const rotation = prefix.getUint32(8, true);
  const width = prefix.getUint32(12, true);
  const height = prefix.getUint32(16, true);
  if (seq > BigInt(Number.MAX_SAFE_INTEGER)) throw BAD_VIDEO;
  if (rotation > MAX_VIDEO_ROTATION || !isSide(width) || !isSide(height)) throw BAD_VIDEO;
  if (width * height > MAX_VIDEO_PIXELS) throw BAD_VIDEO;
  const length = width * height * 4;
  if (bytes.byteLength !== VIDEO_PREFIX_BYTES + length) throw BAD_VIDEO;
  // The buffer is an ArrayBuffer here: the response is never shared memory.
  const pixels = new Uint8ClampedArray(bytes.buffer as ArrayBuffer, bytes.byteOffset + VIDEO_PREFIX_BYTES, length);
  return { seq: Number(seq), rotation, width, height, pixels };
}

/** One video tile as the scheduler tracks it. `key` changes when another device takes the tile. */
export type TileRef = { tile: number; key: string };

type TileState = {
  key: string;
  // The picture number last shown; the next request asks for newer ones.
  seq: number;
  running: boolean;
  again: boolean;
};

/**
 * Fetches pictures for the active tiles. A `poke` (the core says a new
 * picture is waiting) asks each active tile for a picture newer than the last
 * shown one. At most one request runs for each tile; a poke that arrives
 * while one runs makes at most one more run after it. A picture that is not
 * newer than the one shown is dropped. A tile that disappears, or that
 * another device takes, loses its state, and a reply still on its way is
 * ignored. A failed request keeps the picture shown; the next poke asks again.
 */
export class VideoFetcher {
  #fetch: (tile: number, after: number) => Promise<VideoPicture | null>;
  #show: (tile: number, picture: VideoPicture) => void;
  #clear: (tile: number) => void;
  #tiles = new Map<number, TileState>();

  constructor(
    fetch: (tile: number, after: number) => Promise<VideoPicture | null>,
    show: (tile: number, picture: VideoPicture) => void,
    clear: (tile: number) => void = () => {}
  ) {
    this.#fetch = fetch;
    this.#show = show;
    this.#clear = clear;
  }

  /** The tiles with state now, in order. */
  get active(): number[] {
    return [...this.#tiles.keys()].sort((a, b) => a - b);
  }

  /** Replaces the active tiles. Tiles outside 0 to 4 and repeated tiles are ignored. */
  setTiles(tiles: readonly TileRef[]): void {
    const wanted = new Map<number, string>();
    for (const { tile, key } of tiles) {
      if (Number.isInteger(tile) && tile >= 0 && tile < VIDEO_TILES && !wanted.has(tile)) wanted.set(tile, key);
    }
    for (const [tile, state] of [...this.#tiles]) {
      if (wanted.get(tile) !== state.key) {
        this.#tiles.delete(tile);
        this.#clear(tile);
      }
    }
    let added = false;
    for (const [tile, key] of wanted) {
      if (!this.#tiles.has(tile)) {
        // A reply for an earlier state of this tile finds a different object
        // here and is ignored.
        this.#tiles.set(tile, { key, seq: 0, running: false, again: false });
        added = true;
      }
    }
    if (added) this.poke();
  }

  /** Asks every active tile for a newer picture. */
  poke(): void {
    for (const [tile, state] of this.#tiles) {
      if (state.running) {
        state.again = true;
      } else {
        void this.#run(tile, state);
      }
    }
  }

  /** Drops all tiles and ignores replies still on their way. */
  stop(): void {
    for (const tile of [...this.#tiles.keys()]) this.#clear(tile);
    this.#tiles.clear();
  }

  async #run(tile: number, state: TileState): Promise<void> {
    state.running = true;
    try {
      do {
        state.again = false;
        try {
          const picture = await this.#fetch(tile, state.seq);
          if (this.#tiles.get(tile) !== state) return;
          if (picture !== null && picture.seq > state.seq) {
            state.seq = picture.seq;
            this.#show(tile, picture);
          }
        } catch {
          // Keep what is shown; the next poke asks again.
        }
      } while (state.again && this.#tiles.get(tile) === state);
    } finally {
      state.running = false;
    }
  }
}

/** How a picture sits in its tile; all values are numbers the page computed. */
export type TileLayout = {
  // The tile's width over height as it shows, after the quarter turns.
  ratio: number;
  // The canvas width as a percentage of the tile's width.
  canvasPercent: number;
  transform: string;
};

/**
 * Where a picture goes in its tile. A quarter turn swaps the shown sides, so
 * the tile takes the turned ratio, and the canvas (centred, then turned) is as
 * wide as the tile is high. The own camera is mirrored on screen, after the
 * turn, so it looks like a mirror.
 */
export function tileLayout(width: number, height: number, rotation: number, mirrored: boolean): TileLayout {
  const turned = rotation % 2 === 1;
  const ratio = turned ? height / width : width / height;
  const canvasPercent = turned ? (100 * width) / height : 100;
  const turn = (rotation % 4) * 90;
  const flip = mirrored ? ' scaleX(-1)' : '';
  return { ratio, canvasPercent, transform: `translate(-50%, -50%)${flip} rotate(${turn}deg)` };
}
