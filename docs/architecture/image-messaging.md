# Image messaging design

24 October 2026. Status: **proposed design for P5**. It applies
[ADR-0009](../adr/0009-strip-sent-image-metadata-and-load-received-images-on-request.md)
(owner decisions) and the
[development plan](development-plan.md#receive-and-send-images). The limits
come from `crates/media-protocol`. Nothing here is verified until its evidence
passes.

## Scope

* PNG and JPEG only, still images only. An APNG shows its default image.
* One image per message, as `m.image`. No captions, galleries, thumbnails or
  blurhash in this release.
* Windows only, because the contained worker exists only on Windows. On other
  platforms both flows fail closed with `unavailable`.

## Guest operations

The guest in `crates/media-decoder` gains one operation, and the decode
operation gains one output. Both change the ABI together with the sandbox
allowlist (`crates/media-sandbox`) and the worker (`crates/media-worker`). As
built, the guest's host imports are `input_len`, `read_input`, `write_output`
and `write_stripped`; the last returns the stripped copy.

* **Decode** (existing). It also reports the EXIF orientation (1 to 8, or 1
  when absent or invalid) of a JPEG. The host applies the rotation or flip to
  the validated pixels. PNG orientation is always 1.
* **Strip** (new). The input is one encoded file of at most 20 MiB. The output
  is a stripped copy, no larger than the input.
  * JPEG: keep SOI, DQT, DHT, DRI, SOFn, SOS with its entropy-coded data, RSTn
    and EOI. Keep APP14 (Adobe), because it selects the colour transform. Drop
    every other APPn (including JFIF, EXIF, XMP, ICC and Photoshop) and COM.
    When the input had an EXIF orientation other than 1, write one minimal
    APP1 segment that holds only IFD0 with the Orientation tag.
  * PNG: keep IHDR, PLTE, tRNS, IDAT and IEND, copied byte for byte with their
    CRCs. Drop every other chunk, including text, eXIf, iCCP, tIME, pHYs and
    the APNG chunks.
  * The guest decodes the original and the stripped copy and compares a hash
    of the two pixel outputs. A difference, a parse error or a size increase
    rejects the operation. Nothing is sent after a rejection.
* Every parse is bounded: segment and chunk lengths are checked before use,
  and unknown or malformed structure rejects. The guest strips in place, so
  the output is never longer than the input (an EXIF segment that held an
  orientation is at least as long as the 36-byte minimal APP1 that replaces
  it). The sandbox host refuses a longer output.
* Bytes after the JPEG EOI or the PNG IEND are dropped, not rejected. This also
  removes appended data such as motion-photo video. A malformed EXIF segment is
  dropped, and the orientation is then 1.
* The pixel comparison runs in the guest. A guest compromised by the selected
  file could return other bytes, up to the input length, but has no other
  data or capability. The core decodes the stripped copy again in a new
  worker for the preview, so the user sees what will be sent.

The worker selects the operation from a Control frame `[1, op]` sent before the
EncodedImage frame. A decode replies with the RgbaPixels frame and then a
Control frame that holds the orientation. A strip replies with one EncodedImage
frame. The broker exposes `run_decode` and `run_strip`, which share one
launcher, so the containment, deadline and exit-code checks are the same.

Time limits (provisional, to be measured): a strip decodes twice, and recorded
benchmarks show 5 to 8 s for one 16-megapixel decode in the worker. The core
therefore uses a 20 s wall and CPU limit for a decode and 40 s for a strip,
with the default 256 MiB memory limit. These values replace the 10 s starting
proposal for these two operations only after a benchmark is recorded.

## Receiving an image

1. The core projects an `m.image` event as a timeline image item: sender,
   time, event ID, a sanitized file name (bounded, inert text), the declared
   size, width and height (untrusted, clamped, used only for layout), and
   whether the file is encrypted. No media is fetched.
2. The user chooses to load it. The page sends `load_image` with the room ID
   and event ID. The core checks the session, the room and the event, and
   takes a lease.
3. The core downloads the content through the authenticated media API with a
   hard cap of 20 MiB **before** the body is allocated. A larger declared or
   actual size rejects. For an encrypted file, the core decrypts with the SDK's
   attachment decryptor, which checks the SHA-256 hash. It never decrypts with
   its own code.
4. The core passes the decrypted bytes to the contained worker (one decode at a
   time, at most two transfers). It applies the orientation, then scales the
   validated pixels down to fit within 2048 × 2048 pixels (at most 16 MiB of
   RGBA). It stores the result under an opaque, random handle.
5. The reply holds the handle, width and height. The page then fetches the
   pixels in row bands (see below) and draws them on a canvas.

## Pixel transport

The core-to-UI channel carries JSON in Control frames of at most 128 KiB. Pixels
travel in row bands instead:

* The page calls `image_band` with a handle and a band index. The core replies
  with a `pixel_reply` message (request ID, width, rows, band index) in a
  Control frame, and then exactly one `RgbaPixels` frame. Both frames are
  written together under the core's output lock, so no event comes between
  them. The frame's width is the image width, its height is the number of
  rows in the band, and its body is at most 1 MiB. An error replies with an
  ordinary error reply and no pixel frame.
* The UI process reads that frame with the budgeted frame reader and returns
  its bytes to the page as a raw IPC response (`tauri::ipc::Response`), not as
  JSON or base64. The page copies each band into an `ImageData` and draws it
  with `putImageData`. The CSP is not changed: no `blob:` or `data:` images.
* The core keeps decoded pixels in the `RetainedPixels` budget class (64 MiB,
  so at most four maximum-size images), and evicts the least recently used.
  Handles are bound to the session generation. A lock or sign-out drops every
  handle and its pixels; a stale handle returns `not_found`.

## Sending an image

1. The user chooses to attach an image. The page sends `pick_image` with the
   room ID. The core opens the native file dialog (`GetOpenFileNameW`) in its
   own window, filtered to PNG and JPEG, without following shortcuts and
   without adding the file to the recent list. The page never sends a path.
2. The core opens the file read-only, shared for reading only, so no other
   process can write it during the read. It refuses a reparse point (a
   symbolic link or junction), a directory and a file larger than 20 MiB. It
   reads the file once and fails if the length differs from the size it
   checked. It never writes the file.
3. The worker strips the copy. The core then decodes the stripped copy for a
   preview, and returns a preview handle with its width and height.
4. The page shows the preview. The user confirms or cancels. Cancel drops the
   stripped bytes and the preview.
5. On confirm, the core sends the stripped bytes as an attachment through the
   SDK. The SDK encrypts the attachment in an encrypted room. The event carries
   the width, height, size and MIME type, and a generic file name
   (`image.png` or `image.jpg`), so the original file name does not leave the
   device.
6. The same checks as for text apply before sending: joined room, known
   encryption state, consent for an unencrypted room, and the identity-change
   pause (ADR-0007). An encrypted send never falls back to plain text.
7. The outbox holds at most one image at a time, queued or sending. It keeps
   the transaction ID for a retry, and reports the same delivery states as
   text. A retry can upload the file again; Persephone does not promise to
   delete an uploaded object.

## Limits

| Item | Limit |
| --- | --- |
| Encoded file, download or selection | 20 MiB, checked before allocation |
| Decoded image | 8192 per side, 16 megapixels, 64 MiB RGBA |
| Displayed image | Scaled to fit 2048 × 2048 |
| Retained pixels | 64 MiB, least recently used evicted |
| Band | At most 1 MiB of RGBA |
| Concurrent work | 1 decode, 2 transfers, 1 image in the outbox |

## Implementation notes

Recorded 24 October 2026 from the implementation on `p5/image-messaging`.

* The pixel reply is its own Control message, `{"pixel_reply":{...}}`, not a
  `CoreMessage` variant. The UI process reads core output with
  `receive_from_core`, which checks the pixel header against the reply before
  it allocates the body.
* `ImageView` also carries `bandRows`, so the page can place a short last band.
  A declared `size` is a `u32`.
* A new error code, `rejected`, covers files that are too large, malformed,
  not PNG or JPEG, fail the hash check, or that the worker refuses, and media
  that the server refuses (a 4xx status other than 404 or 429).
* The guest decodes a JPEG only if its structure is complete (every scan ends
  at a marker, and EOI is present), and with zune-jpeg's strict mode. A
  transfer that stops early is refused.
* matrix-sdk 0.19.1 buffers a whole media download without a limit, so the
  core sends the request itself. It uses the authenticated
  `/_matrix/client/v1/media/download` endpoint when the server versions
  support it, as the SDK does, and otherwise the legacy
  `/_matrix/media/v3/download` endpoint without the token.
* `load_image` and `pick_image` run outside the serving loop and reply later.
  At most two loads and one pick run at a time, and they share one worker.

Known limits of this implementation:

* In an encrypted room, the SDK uploads the encrypted file before it checks
  the room keys. A send from an unverified device can therefore leave an
  encrypted object on the server, and each retry uploads again.
* `FILE_FLAG_OPEN_REPARSE_POINT` checks only the last path component. A
  junction earlier in the chosen path is followed.
* A download holds a lease. A lock can wait for the current chunk, up to the
  60 s download timeout, and replies `locking` until then.
* Decoded pixel frames and the SDK's copy of an upload are not zeroed when they
  are dropped.
* An open file dialog keeps the core from ending until the dialog closes.
* The UI process limits concurrent `image_band` calls to the core (four), but
  it does not count band responses that wait for delivery to the web view. A
  page that asks for bands faster than it reads them can make the UI process
  hold more. This affects only the UI process, which has its own memory limit
  (ADR-0004); the core's budgets are unchanged.
* zune-jpeg 0.5.15 fills the rest of a scan with zeros when it meets a valid
  marker early, also in strict mode. A file cut inside a scan and then given a
  new EOI marker therefore decodes with blank final blocks. This needs a
  deliberately made file; the sender could send such an image anyway.
* With a debug build of the worker, a 12-megapixel JPEG exceeds both time
  limits. The release worker fits them (see the
  [image evidence](../evidence/image-messaging.md#timing)).

## Required evidence

* Malformed, oversized, truncated, wrong-type and cancelled images, both sent
  and received, with safe errors and no partial display.
* Strip tests: each dropped segment type is gone, kept segments are byte for
  byte equal, orientation is kept, and pixels are unchanged.
* A received image in an encrypted and an unencrypted room, on both
  homeservers, in Sliding and classic sync.
* A sent image opens in Element Web in an encrypted room.
* A lock during load or send discards the result; a stale handle fails.
* The source file is unchanged after sending (hash before and after).
