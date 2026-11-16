# Accept a pasted image from the page as hostile bytes

* Status: accepted
* Deciders: project owner
* Date: 2026-11-16

## Context and Problem Statement

Users expect to paste an image, such as a screenshot, into the message box,
see it attached, and send it. Under
[ADR-0009](0009-strip-sent-image-metadata-and-load-received-images-on-request.md),
the core reads a sent image only from its own file dialog, and the page never
sends a path or image bytes. A pasted image exists only on the clipboard. The
web view receives it as a file in the paste event. How can a pasted image
reach the core without new authority for the page and without a new decoder
outside the contained guest?

## Decision Drivers

* Keep the web view and the UI process keyless (ADR-0004), and give the page
  no generic file, clipboard or network capability.
* Decode only inside the contained WebAssembly guest (ADR-0003), and strip
  metadata as for a picked file (ADR-0009).
* Bound sizes, queues and concurrent work before allocation, in each process
  that receives the bytes.
* Do not add an encoder or a new native dependency.
* Keep the confirm step: an attached image is sent only when the user
  presses Send.

## Considered Options

* The page sends the pasted bytes to the core.
* The core reads the operating system's clipboard itself.
* Do not support paste.

## Decision Outcome

Chosen option: "The page sends the pasted bytes to the core" (owner decision,
16 November 2026), because it adds no new authority to the page. The page can
already send any text into a room. The bytes take the same path as a picked
file after the read: the strip in the guest, the preview decode in the guest,
a draft, and a send only on confirm.

* The page attaches an image only from a paste event that holds one PNG or
  JPEG file and no plain text. Office programs also put a picture of copied
  text on the clipboard, so a paste with plain text stays text. The page
  refuses an empty file, a file over 20 MiB and other types before it reads
  the file.
* The page calls `paste_image` with the bytes as the raw request body and the
  room ID in the `persephone-room-id` header. The UI process refuses a JSON
  body, a missing or invalid room header, an empty body and a body over
  20 MiB. It relays one paste at a time.
* The UI process writes a `paste_image` control frame and, at once and under
  the same writer lock, one `EncodedImage` frame of the declared length.
* The core reads the image frame's header and checks its kind and length
  against the request before it reserves the `EncodedInFlight` budget and
  allocates the body. A missing frame, another kind or another length ends
  the connection as a protocol violation. If the budget is full, the core
  reads past the bytes without keeping them and replies `limit_exceeded`.
* The core treats the bytes as hostile. It applies the checks of a pick (a
  joined room, the session generation, one pick at a time, at most one
  waiting draft), then strips and decodes them in the guest.

### Positive Consequences

* Pasting a screenshot works without a file dialog.
* No new decoder, encoder or native clipboard code. The page gets no clipboard
  read capability.
* A pasted image loses its metadata like a picked one, and the original file
  name is never known.

### Negative Consequences

* The UI-to-core channel now carries up to 20 MiB of image bytes in one frame,
  not only control frames. The core's reader handles one more frame kind.
* Tauri allocates the whole request body in the UI process before the host
  command can check its length. The page checks the size before it reads the
  file, but a compromised page could still make the UI process allocate a
  large body. The UI process has its own memory limit (ADR-0004), and the
  core's budgets are unchanged.
* The UI process holds a second copy of the image while it writes the frames.
* Clipboard images that are not PNG or JPEG, such as GIF or WebP, cannot be
  pasted.

## Pros and Cons of the Options

### The page sends the pasted bytes to the core

* Good, because it reuses the guest strip and decode path unchanged.
* Good, because a compromised page gains nothing it could not already send.
* Bad, because a larger body crosses the UI process and the core channel.

### The core reads the operating system's clipboard itself

* Good, because no image bytes cross from the page.
* Bad, because it needs native clipboard code and new Windows API features
  for each OS.
* Bad, because Windows screenshots arrive as device-independent bitmaps,
  which need an encoder to send as PNG (ADR-0009 avoids one).
* Bad, because a compromised page could make the core read the clipboard
  image without a paste.

### Do not support paste

* Good, because nothing changes.
* Bad, because a common way to share a screenshot is missing.

## Links

* Refines [ADR-0009](0009-strip-sent-image-metadata-and-load-received-images-on-request.md)
* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)
* [Image messaging design](../architecture/image-messaging.md#pasting-an-image)
