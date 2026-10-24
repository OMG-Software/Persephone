# Image messaging with Element Web

Checked 24 October 2026 on Windows 11 Pro x64 (build 26300), debug builds of
Persephone at commit `9fbf074` (Rust 1.99.1, Matrix Rust SDK 0.19.1), against
disposable Merovingian 0.12.29 and Synapse 1.163.0 in WSL Ubuntu 24.04. The
other client was Element Web v1.12.32 (its crypto: Rust SDK 0.19.1) in the
Claude desktop app's built-in browser (Chromium). Timing also used a release
build of the media worker. This is a live acceptance run of the
[image messaging design](../architecture/image-messaging.md) and
[ADR-0009](../adr/0009-strip-sent-image-metadata-and-load-received-images-on-request.md).
No product code changed.

**Result: on both servers, received images stayed placeholders with no media
request until "Load image", then drew correctly rotated; sent images reached
Element as encrypted `m.image` events with a `file`, a generic name and no
metadata except a minimal EXIF orientation; the source files were unchanged;
the unencrypted room asked for consent first and sent a `url`; a lock during a
load discarded the image. One failure: a truncated JPEG from the other client
is drawn as a partial image (step 4a, both servers). With the debug worker a
12-megapixel JPEG exceeds both time limits; with the release worker it fits
them.**

## Inputs

| Item | Value |
| --- | --- |
| Persephone | `persephone.exe` with `--features dev-disposable-servers`, `persephone-ui`, `persephone-media-worker` (debug and release), release `persephone_media_decoder.wasm`, and `p4_peer`, built from the clean worktree at `9fbf074` with `--locked --offline`. |
| Element | The verified, unpacked v1.12.32 from the [Element Web run](element-web-interop.md). This run's configuration turns image display on (`showImages: true`, GIF and video autoplay off) and keeps integrations, widgets, VoIP, Element Call, URL previews, identity server, analytics, registration and federation off. |
| Test images | Synthetic, made with Pillow 12.2.0 by `gen_images.py` (in `.local`). Every stored image has a red block at its top-left and a blue block at its bottom-right, so rotation can be read from pixels. Metadata carries the marker `SECRET-METADATA-p5`, a fake camera name `FakeCam` and a GPS IFD at 0,0. |

| File | Bytes | Content |
| --- | --- | --- |
| `recv-orient6.jpg` | 1,209 | stored 96 × 48, EXIF orientation 6, GPS, text tags |
| `recv.png` | 206 | 64 × 40, `tEXt` |
| `send-orient6.jpg` | 1,714 | stored 120 × 60; segments XMP APP1, COM, JFIF APP0, EXIF APP1 (orientation 6, ImageDescription, Make, Model, GPS IFD pointer `0x8825`) |
| `send.png` | 617 | 80 × 50; chunks `tEXt`, `iTXt`, `zTXt`, `eXIf` (GPS, text tags), `tIME` |
| `plain-send.png`, `plain-recv.jpg` | 163, 1,154 | no metadata |
| `hostile-truncated.jpg` | 1,054 | the first 60 % of a valid 200 × 120 JPEG |
| `hostile-gif-as-png.gif` | 153 | a GIF, sent with MIME type `image/png` |
| `hostile-21mib.png` | 22,020,104 | PNG signature, then random bytes |
| `noise-12mp-baseline.jpg` | 7,216,651 | 4000 × 3000 random noise, quality 75, baseline |
| `noise-12mp-progressive.jpg` | 6,578,506 | the same, progressive |

The analyser (`analyze.py`, in `.local`) lists JPEG segments or PNG chunks,
the EXIF IFD0 tags, and counts of `SECRET-METADATA-p5`, `FakeCam`,
`http://ns.adobe.com/xap`, `xmpmeta`, `comment ` and `Exif\0\0`. As a negative
control, on `send-orient6.jpg` it found the marker 5 times, `FakeCam`, the XMP
namespace, `xmpmeta`, the COM text and the GPS IFD pointer; on `send.png` the
marker 5 times and `FakeCam` (the `zTXt` copy is compressed).

## Method

The set-up was the same as in the [Element Web run](element-web-interop.md):
Persephone signed in as `p0_alice` ("A") and set up encryption; Element signed
in as `p0_bob` ("B") on a new origin per server and set up cross-signing;
Element created the direct-message room with its first message; the SDK peer
(`login alice`, `alice-join-dm <room>`, `quit`) joined it for A, because
Persephone cannot accept invitations. A and B verified each other by emoji
(started from Persephone's header; same 7 emoji on both sides; both
confirmed; Persephone showed the contact `verified`, Element
`isCrossSigningVerified() = true`). The driver `pctl.py` (extended from the
earlier run) acts on Persephone's page through WebView2's DevTools port and
reads its canvases.

New for this run:

* **Request log.** A signed in through a small loopback relay (`proxy.py`)
  that logs the method and path of every HTTP request, without query,
  headers or body. Merovingian does not log requests; on Synapse its access
  log was checked as well.
* **Picking.** `PERSEPHONE_DEV_PICK_FILE` named a `.local` answer file that
  held the image path before each "Attach image".
* **Element side.** B's images were pasted into Element's composer as files
  (Element's own upload path, encrypted by Element) for steps 1 and 5. Hostile
  and timing images were sent with B's client object (`mxMatrixClientPeg`):
  the page encrypted them with WebCrypto in the attachment v2 layout
  (AES-256-CTR, SHA-256), uploaded them and sent the `m.image` event, which
  the client encrypted. The checks of A's sent files downloaded them with B's
  token from the authenticated media API, decrypted them in the page, checked
  the hash, and saved the plain bytes to `.local` through the static server.

## Results

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| Set-up and emoji verification | pass | pass |
| 1. Receive, click to load | pass | pass |
| 2. Send in the encrypted room | pass, but Element's own display not observed (limit 1) | pass, same limit |
| 3. Unencrypted room | pass (first image from B quarantined by the server, finding 3) | pass |
| 4a. Truncated JPEG | **fail: partial image drawn** | **fail: partial image drawn** |
| 4b. GIF declared as PNG | pass | pass |
| 4c. Encrypted file with a wrong hash | pass | pass |
| 4d. Declared size 25 MiB | pass | pass |
| 4e. Actual size 21 MiB, declared 1,000 bytes (extra) | pass | pass |
| 4. No crash, other messages work | pass | pass |
| 4. Send side: truncated, GIF, 21 MiB, cancel (extra) | pass | not run |
| 5. Classic sync, receive | not applicable | pass (classic-only Synapse) |
| 6. Lock during load | pass (release worker) | pass (lock with debug worker, reload with release worker) |
| 7. Timing | see [Timing](#timing) | 3 more load samples |
| Servers stopped | runner exit 0 | runner exit 0 (both instances) |

### 1. Receive, click to load

Both files from Element arrived as `m.room.encrypted` events holding `m.image`
with `file` (v2). Persephone's timeline showed for each a placeholder with
the sender's name, declared size, "Encrypted" and "Load image":
`recv-orient6.jpg · 48 × 96 · 1 KB`, `recv.png · 64 × 40 · 206 bytes`.

Before the first click, the relay log held no `/media/` request of A (0 on
each server; on Synapse the access log also held no media request of
`@p0_alice`). Each click caused exactly one
`GET /_matrix/client/v1/media/download/...` (relay and Synapse log, at the
click time). The JPEG drew on a 48 × 96 canvas with red at the top-right and
blue at the bottom-left, which is the stored image turned 90° clockwise
(orientation 6). The PNG drew 64 × 40 with red top-left and blue
bottom-right. Screenshots are in `.local` only.

### 2. Send in the encrypted room

| Check | Observed (same on both servers) |
| --- | --- |
| Preview | JPEG: "JPEG · 60 × 120 · 1 KB", canvas 60 × 120 upright (red top-right). PNG: "PNG · 80 × 50 · 184 bytes", unchanged orientation. |
| Requests | Per image: `POST /_matrix/media/v3/upload`, then `PUT .../send/m.room.encrypted/...`. Nothing was uploaded before "Send image". |
| Wire type (B) | `m.room.encrypted`, no decryption failure |
| Decrypted content (B) | `msgtype: m.image`; keys `body`, `file`, `info`, `msgtype`; no `url`; `file` has `hashes`, `iv`, `key`, `url`, `v` (`v2`). `body` is `image.jpg` / `image.png`. `info`: `mimetype`, `size` (1,176 / 184), `w`/`h` (60 × 120 / 80 × 50), `is_animated: false`. |
| Download (B) | Authenticated `/_matrix/client/v1/media/download`; ciphertext 1,176 / 184 bytes; hash check passed. |
| JPEG bytes | Segments `APP1 Exif (34 bytes)`, `DQT`, `DQT`, `SOF0`, `DHT` × 4, `SOS`, `EOI`. EXIF IFD0 holds one tag, Orientation (`0x0112`) = 6, next IFD 0, no GPS pointer. No `SECRET-METADATA-p5`, `FakeCam`, XMP namespace, `xmpmeta`, COM or JFIF. Bytes from the first DQT to the end equal the source byte for byte. Decoded pixels equal the source's. |
| PNG bytes | Chunks `IHDR`, `IDAT`, `IEND` only, byte-equal to the source chunks; no marker or `FakeCam`. Decoded pixels equal the source's. |
| Same result on both servers | The received files had the same SHA-256 on both servers (JPEG `221175d8…`, PNG `13b49a13…`). |
| Upright in a browser | Decoded by Chromium from B's decrypted bytes: JPEG 60 × 120 with red top-right, PNG 80 × 50 unchanged. |
| Source files | SHA-256 before and after sending equal (`8964c844…`, `63aa301e…`). At the end of the run all 12 test images matched the manifest. |

### 3. Unencrypted room

B created the room with the `public_chat` preset and private visibility, and
the peer joined it for A (this also marks it direct for A). With the
`private_chat` preset Merovingian added an `m.room.encryption` event itself
(finding 4), so that room was not used.

After "Send image", Persephone showed "This room is not encrypted: … Send
unencrypted messages in this room, starting with this image?" and made no
upload. After "Send unencrypted", it uploaded and sent `m.room.message`; B
saw `m.image` with `url` and no `file`, `body: image.png`, `info` with 72 × 36,
163 bytes, `image/png`. The bytes B downloaded equal the metadata-free source.
B then sent `plain-recv-2.jpg` with a `url`; A loaded it (50 × 100, red
top-left).

On Merovingian, B's first unencrypted image was uploaded as
`application/octet-stream`. Its download returned HTTP 451 (`media is
quarantined`) to A and to B. Persephone showed "The image could not be
loaded. Images work on Windows only for now." and a "Try again" button
(finding 2). The image re-sent with `Content-Type: image/jpeg` loaded.

### 4. Hostile input

B sent each image as an encrypted file in the encrypted room. A clicked
"Load image" on each in turn.

| Case | Request by A | Persephone |
| --- | --- | --- |
| a. truncated JPEG | one download | **A 200 × 120 canvas was drawn: the top rows show the image (red block), the rest is RGBA (128, 128, 128, 128).** No error shown. |
| b. GIF declared `image/png` | one download | "This image cannot be shown: it is too large, damaged, or not a PNG or JPEG." No canvas. |
| c. wrong ciphertext hash | one download | same message, no canvas |
| d. declared size 26,214,400 | **none** | same message, no canvas |
| e. 22,020,104 bytes, declared 1,000 | one download | same message, no canvas |

The core and UI processes kept running. Afterwards A sent and B read
`p5-M1` / `p5-S1`, B sent and A read `p5-M2` / `p5-S2`, and A's earlier
`image.jpg` loaded again.

On Merovingian, the same files picked for sending (send side) were refused
with `rejected` ("That file cannot be sent: …"): truncated JPEG after 0.8 s,
GIF after 0.7 s, 21 MiB file after 5 ms. An empty answer file returned
`cancelled`. So the truncated JPEG is refused when sent, but drawn when
received.

### 5. Classic sync

Synapse was started again with `--synapse-classic-only` (new accounts, new
Persephone profile, new Element origin). Its `/versions` listed
`org.matrix.simplified_msc3575: false`; the relay log of A held only
`GET /_matrix/client/v3/sync` (35 requests) and no Sliding Sync request. Both
images from Element showed as placeholders, with no media request before the
click. After the clicks, the JPEG drew 48 × 96 with red top-right, the PNG
64 × 40. Emoji verification was not repeated on this instance.

### 6. Lock during load

B sent a 12-megapixel progressive JPEG (6.6 MB). A clicked "Load image" and
then "Lock".

* Merovingian, release worker: Lock 1.5 s after the click (the download
  request had been answered). The store state was `restart_required` within
  4 s; the page showed "Locked: restart Persephone to unlock".
* Synapse, debug worker: Lock 3.7 s after the click; `restart_required`
  within 3 s. The worker process was still running when checked a few
  seconds after the lock, and had ended about 20 s later.

In this build an unlock after a sign-in needs a restart. After the restart
(with the release worker), the room showed the image as a placeholder with
"Load image" and no canvas. Loading it again drew it at 2048 × 1536 (8.7 s on
both servers). A stale handle was not tested separately.

### Timing

From "Load image" to a drawn canvas (polled every 0.5 s), and the
`pick_image` round trip (read, strip, preview decode and scale) for the
noise JPEGs. The core was the debug build in all cases.

| Operation | Debug worker | Release worker |
| --- | --- | --- |
| Load, 12 MP baseline | rejected after 21.1 s (decode limit 20 s) | 6.2, 6.2 s (Merovingian), 6.7 s (Synapse) |
| Load, 12 MP progressive | rejected after 21.1 s | 8.2, 8.2, 8.7 s (Merovingian), 8.7, 8.7 s (Synapse) |
| Strip and preview, 12 MP baseline | rejected after 40.1 s (strip limit 40 s) | 14.1, 14.1 s |
| Strip and preview, 12 MP progressive | rejected after 40.1 s | 21.0, 20.9 s |

Loads include a 6.6 to 7.2 MB loopback download and decryption in the debug
core. Loaded images were scaled to 2048 × 1536. Inference: with the release
worker the decode and strip fit the 20 s and 40 s limits for these files,
with a margin of about 2 for a strip-and-preview of the progressive file
(its strip and preview were not timed separately). The debug worker does not
fit either limit at 12 megapixels.

### Findings

1. **A truncated received JPEG is drawn as a partial image (Persephone
   defect).** On both servers, `load_image` succeeded for the first 60 % of a
   valid JPEG; the canvas showed the decoded top rows and fill pixels of
   RGBA (128, 128, 128, 128) for the rest. The design's required evidence
   asks for "safe errors and no partial display" for truncated images. The
   send path refused the same file. Not fixed in this run.
2. **A non-404 download failure shows a misleading text (Persephone
   observation).** An HTTP 451 reply became `unavailable`
   (`core_unavailable` on the page), which the page shows as "The image could
   not be loaded. Images work on Windows only for now." on Windows. The error
   is safe; the text names the wrong cause.
3. **Merovingian quarantined an unencrypted upload** whose declared content
   type (`application/octet-stream`) did not match its JPEG content, and
   answered downloads with HTTP 451. Inference: content sniffing. A server
   behaviour, not a Persephone defect. Synapse was not tested with that
   upload.
4. **Merovingian encrypts `private_chat` rooms by default**: a room created
   without an encryption state event received one from the server, attributed
   to the creator.
5. **Classic sync: the direct-message room had no contact.** On the
   classic-only Synapse, the room the peer marked as direct was listed with
   `contact: null`, and `contact_status` returned `not_found`; with Sliding
   Sync both servers showed the contact. Not investigated.
6. **A lock does not stop a running worker.** The debug worker kept running
   after the lock until about its time limit; its result was not shown after
   the restart.

### Attempts not counted

* Send-side picks through a shell loop first named a wrong path (a quoting
  error in the operator's script) and returned `not_found`; they were rerun
  with the correct path.
* At the end of the classic-only run, closing the window with `taskkill`
  (WM_CLOSE) left the UI process running without a window; the core printed
  `the window failed: WebViewGone` and showed its retry dialog. Cancel ended
  the core with exit 1. All steps on that instance had finished. RTSS and MSI
  Afterburner were running, as in the earlier runs.

## Commands

From the repository root. `<…>` are placeholders; credentials stay in the
harness `profile.json` under `.local/`. The frontend steps used
`npm_config_cache=.local/cache/npm`, as `tools/dev.py` does.

```text
npm ci --no-audit --no-fund --offline ; npm run build            # in frontend/
cargo build --locked --offline --release -p persephone-media-decoder --target wasm32-unknown-unknown
cargo build --locked --offline -p persephone-media-worker -p persephone-desktop -p persephone-test-matrix --bins
cargo build --locked --offline -p persephone-core --features dev-disposable-servers --bin persephone
cargo build --locked --offline --release -p persephone-media-worker

python -E gen_images.py <images dir>
wsl -d Ubuntu-24.04 -u root -- /usr/bin/setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all --reuid=1000 --regid=1000 --init-groups --reset-env --no-new-privs /usr/bin/python3 /mnt/c/dev/Persephone/tools/servers/run.py --merovingian-binary /mnt/c/dev/Persephone/.local/servers/merovingian/merovingian-server --synapse-python /mnt/c/dev/Persephone/.local/servers/synapse/bin/python --server <merovingian|synapse> [--synapse-classic-only] --output /mnt/c/dev/Persephone/.local/runs/p5-images-01/<dir>
python -I proxy.py <relay port> <server port> <relay.log>
python -I serve.py .local/element-web/unpacked-v1.12.32/element-v1.12.32 <config.json> <port> <images dir> <save dir>

PERSEPHONE_DEV_PROFILE=<profile copy naming the relay> PERSEPHONE_DEV_PASSPHRASE_FILE=<.local file>
PERSEPHONE_DEV_RECOVERY_FILE=<.local file> PERSEPHONE_DEV_PICK_FILE=<.local answer file>
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>
.local/build/rust/debug/persephone.exe --profile <new .local directory>
<.local copy of persephone.exe and persephone-ui.exe next to the release worker> --profile <same directory>

python -E -P -X utf8 pctl.py <port> signin|setup|open|click|clicklabel|clickin|loadtime|canvases|draft|images|timed pick_image|shot ...
p4_peer.exe <profile.json> <server>   < login alice / alice-join-dm <room> / quit
python -E analyze.py <file>...
taskkill /IM persephone-ui.exe ; wsl -d Ubuntu-24.04 -u root -- kill -TERM <runner pid>
```

Raw output, scripts, relay logs, saved received files, the image manifest and
Persephone screenshots are in `.local/runs/p5-images-01`. Element's browser
storage for each origin was deleted at the end; for the Merovingian and
Synapse origins this was checked afterwards (no session, no crypto store).

## Limits

1. **Element did not display A's images.** The built-in browser refused to
   register Element's service worker ("Failed to register a ServiceWorker …
   unknown error"), so Element requested A's media from the legacy
   unauthenticated endpoint, got 404 and showed "Error downloading image" on
   both servers. B's own uploads displayed from Element's cache. "Element
   shows the image upright" is therefore replaced by B's client decrypting
   the file and Chromium decoding it; the requirement "a sent image opens in
   Element Web" is not shown by this run.
2. Windows only, one host, debug core. Timing used a debug core with a
   release worker; a full release build was not measured. Load times include
   the download and are polled at 0.5 s.
3. Hostile and timing images were sent through B's client object with
   page-side encryption, not through Element's upload UI. Case c tests a
   wrong hash on the whole ciphertext only.
4. Send-side hostile files were tried on Merovingian only. Step 3 used one
   PNG from A and one JPEG from B per server. Emoji verification was not
   repeated on the classic-only Synapse.
5. Step 6 used a restart to unlock, because this build needs one after a
   sign-in. A stale-handle request was not sent.
6. The relay logs only request lines it can see in plaintext loopback HTTP;
   it shows what A requested, not what Element or other clients did.
7. Not rerun. The documentation index, the verification ledger
   (`verification.json`) and the design status are not updated by this
   report.

## Correction after the run

Added 24 October 2026, after this run. **Not a live rerun.** Both findings
from step 4a and the HTTP 451 text were fixed on `p5/image-messaging` after
`9fbf074`:

* The guest now decodes a JPEG only if its structure is complete (every scan
  ends at a marker, and EOI is present), and with zune-jpeg's strict mode.
  Automated tests cut each JPEG fixture at every byte offset, remove the EOI
  and replace scan data with garbage. All are refused, also through the real
  guest, and through the contained worker (`GuestRejected(2)`, no pixels).
  Valid fixtures decode to the same pixels as before. A file cut inside a scan
  and then given a new EOI still decodes with blank final blocks (a zune-jpeg
  limit, recorded in the [design](../architecture/image-messaging.md)).
* A media download answered with a 4xx status other than 404 or 429 now gives
  `rejected`, and the page no longer says that images work on Windows only
  when it runs on Windows.

Step 4a was not run again against the homeservers. The result table above
records the run at `9fbf074`.
