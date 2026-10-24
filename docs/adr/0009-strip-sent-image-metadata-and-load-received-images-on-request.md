# Strip sent image metadata and load received images on request

* Status: accepted
* Deciders: project owner
* Date: 2026-10-24

## Context and Problem Statement

The first release must send and receive images (P5). Photos often carry
metadata that the user does not see, such as GPS location, device serial
numbers, capture times and editing history. Received images are hostile input,
and fetching one tells the homeserver (and, for remote media, other servers)
that the user opened it. Persephone needs a rule for what a sent image contains,
and for when a received image is downloaded and decoded.

## Decision Drivers

* Do not disclose metadata that the user did not choose to share.
* Never change the user's source file.
* Do not lower image quality, and do not add a codec dependency without need.
* Decode only inside the contained WebAssembly guest (ADR-0003).
* Do not fetch or decode hostile media without a user action.

## Considered Options

* Sent images: strip metadata in the guest; decode and encode the pixels again;
  send the original bytes with a warning.
* Received images: load each image when the user asks; load automatically from
  cross-signed senders in encrypted rooms.

## Decision Outcome

Chosen options (owner decisions, 24 October 2026):

* **Strip metadata in the guest.** The WebAssembly guest removes metadata
  segments from a copy of the selected PNG or JPEG file. It checks that the
  stripped copy decodes to the same pixels as the original, and Persephone
  sends only that copy. The selected file is read once and never written.
* **Load received images on request.** Each received image shows a
  placeholder. Persephone downloads, decrypts and decodes it only when the user
  chooses to load it.

The [image messaging design](../architecture/image-messaging.md) gives the
segments that are kept, the transport and the limits.

### Positive Consequences

* Location and device metadata do not leave the device with a sent image.
* Pixels are sent unchanged, with no quality loss and no encoder dependency.
* Opening a room does not fetch or decode any image.

### Negative Consequences

* The strip code is a new parser of the user's file. It runs inside the guest,
  which bounds the effect of a bug.
* Removing EXIF also removes the orientation tag. The design keeps orientation
  in a minimal EXIF segment; without it, phone photos could appear rotated.
* Removing colour profiles can change colours slightly in other clients.
* Click-to-load adds one action for each image.

## Links

* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)
* [Image messaging design](../architecture/image-messaging.md)
* [Development plan: receive and send images](../architecture/development-plan.md#receive-and-send-images)
