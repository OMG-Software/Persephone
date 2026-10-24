// Images (ADR-0009). The core decodes images in its contained worker and holds
// the pixels under an opaque handle; the page fetches them in row bands and
// draws them on a canvas. Nothing here decodes an image: the bytes are RGBA
// pixels already checked by the core and the host. They are checked again
// here, because a canvas is drawn only from a complete, consistent buffer.
import type { ImageMime, ImageView } from '../../crates/contracts/generated/contracts';

/** The raw `image_band` response starts with width and rows, each a little-endian u32. */
export const BAND_PREFIX_BYTES = 8;
/** The core scales every shown image to fit 2048 × 2048. */
export const MAX_SIDE = 2048;
/** Most RGBA bytes in one band. */
export const MAX_BAND_BYTES = 1024 * 1024;
/** Most decoded timeline images the page keeps drawn at once. */
export const MAX_SHOWN_IMAGES = 4;

/** The error a band or view outside the contract gives; mapped like the host's codes. */
export const BAD_IMAGE = 'bad_image';

export type Band = { width: number; rows: number; pixels: Uint8Array };

/** A picked image the user has not sent yet. */
export type ImageDraft = { roomId: string; preview: ImageView; size: number; mime: ImageMime };

const isSide = (value: number) => Number.isInteger(value) && value >= 1 && value <= MAX_SIDE;

/** Reads one raw band response. Throws `BAD_IMAGE` unless it is exactly the prefix and its pixels. */
export function parseBand(raw: unknown): Band {
  let bytes: Uint8Array;
  if (raw instanceof ArrayBuffer) {
    bytes = new Uint8Array(raw);
  } else if (ArrayBuffer.isView(raw)) {
    bytes = new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength);
  } else {
    throw BAD_IMAGE;
  }
  if (bytes.byteLength < BAND_PREFIX_BYTES) throw BAD_IMAGE;
  const prefix = new DataView(bytes.buffer, bytes.byteOffset, BAND_PREFIX_BYTES);
  const width = prefix.getUint32(0, true);
  const rows = prefix.getUint32(4, true);
  if (!isSide(width) || rows < 1) throw BAD_IMAGE;
  const length = width * rows * 4;
  if (length > MAX_BAND_BYTES || bytes.byteLength !== BAND_PREFIX_BYTES + length) throw BAD_IMAGE;
  return { width, rows, pixels: bytes.subarray(BAND_PREFIX_BYTES) };
}

/** True for a view the page can draw: in the documented limits, with consistent bands. */
export function validView(view: ImageView): boolean {
  return (
    /^[0-9a-f]{32}$/.test(view.handle) &&
    isSide(view.width) &&
    isSide(view.height) &&
    Number.isInteger(view.bandRows) &&
    view.bandRows >= 1 &&
    view.bandRows * view.width * 4 <= MAX_BAND_BYTES &&
    view.bands === Math.ceil(view.height / view.bandRows)
  );
}

/**
 * Fetches every band of `view`, in order and one at a time, and copies each
 * into one RGBA buffer at its row offset. Throws `BAD_IMAGE` for a band of the
 * wrong width, row count or length, so nothing partial is drawn. Returns null
 * when `stopped` says the result is no longer wanted.
 */
export async function assembleImage(
  view: ImageView,
  fetchBand: (index: number) => Promise<Band>,
  stopped: () => boolean = () => false
): Promise<Uint8ClampedArray<ArrayBuffer> | null> {
  if (!validView(view)) throw BAD_IMAGE;
  const rowBytes = view.width * 4;
  const pixels = new Uint8ClampedArray(new ArrayBuffer(rowBytes * view.height));
  for (let index = 0; index < view.bands; index += 1) {
    if (stopped()) return null;
    const band = await fetchBand(index);
    const first = index * view.bandRows;
    const rows = Math.min(view.bandRows, view.height - first);
    if (band.width !== view.width || band.rows !== rows || band.pixels.byteLength !== rows * rowBytes) {
      throw BAD_IMAGE;
    }
    pixels.set(band.pixels, first * rowBytes);
  }
  return stopped() ? null : pixels;
}

/**
 * Keeps at most `limit` timeline images drawn. Admitting one more evicts the
 * one admitted first: its `evict` callback drops its canvas, and the item
 * shows its placeholder again.
 */
export class ShownImages {
  #shown: { key: symbol; evict: () => void }[] = [];

  constructor(readonly limit: number = MAX_SHOWN_IMAGES) {}

  admit(evict: () => void): symbol {
    const key = Symbol('image');
    this.#shown.push({ key, evict });
    while (this.#shown.length > this.limit) this.#shown.shift()?.evict();
    return key;
  }

  forget(key: symbol) {
    this.#shown = this.#shown.filter((entry) => entry.key !== key);
  }

  get count(): number {
    return this.#shown.length;
  }
}

/** The page's one set of drawn timeline images. */
export const shownImages = new ShownImages();

// Error codes from the host map to fixed, short messages; nothing else is shown.
const loadText: Record<string, string> = {
  rejected: 'This image cannot be shown: it is too large, damaged, or not a PNG or JPEG.',
  limit_exceeded: 'Other images are loading. Try again when they finish.',
  not_found: 'This image is no longer available.',
  locked: 'The session is locked.',
  invalid_argument: 'This image cannot be loaded.',
  [BAD_IMAGE]: 'The image data was not valid, so it is not shown.'
};

export function loadErrorText(reason: unknown): string {
  return loadText[String(reason)] ?? 'The image could not be loaded. Images work on Windows only for now.';
}

/** The core gives the same answer again for a rejected image. */
export function canRetryLoad(reason: unknown): boolean {
  return String(reason) !== 'rejected';
}

const attachText: Record<string, string> = {
  rejected: 'That file cannot be sent: it is larger than 20 MB, damaged, or not a PNG or JPEG.',
  limit_exceeded: 'Persephone is already preparing an image. Wait for it to finish.',
  not_found: 'That room no longer exists.',
  locked: 'The session is locked.',
  invalid_argument: 'That request was not valid.'
};

export function attachErrorText(reason: unknown): string {
  return attachText[String(reason)] ?? 'The image could not be attached. Images work on Windows only for now.';
}

const sendText: Record<string, string> = {
  limit_exceeded: 'An image is already waiting to send. Wait until it is sent, or discard it, then send this one.',
  not_found: 'The attached image is no longer available. Attach it again.',
  locked: 'The session is locked.',
  invalid_argument: 'That request was not valid.'
};

export function sendImageErrorText(reason: unknown): string {
  return sendText[String(reason)] ?? 'The image was not queued. Try again.';
}

/** A byte count as short text. */
export function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '';
  if (bytes < 1000) return `${Math.round(bytes)} bytes`;
  if (bytes < 1_000_000) return `${Math.round(bytes / 1000)} KB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

/** "640 × 480 · 47 KB" from the sender's claims, or less when one is missing. */
export function declaredText(size: number | null, width: number | null, height: number | null): string {
  const parts: string[] = [];
  if (width !== null && height !== null) parts.push(`${width} × ${height}`);
  if (size !== null) parts.push(formatSize(size));
  return parts.filter((part) => part !== '').join(' · ');
}

export const mimeText: Record<ImageMime, string> = { 'image/png': 'PNG', 'image/jpeg': 'JPEG' };
