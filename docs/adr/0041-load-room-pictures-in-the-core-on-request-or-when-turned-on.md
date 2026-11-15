# Load room pictures in the core, on request or when turned on

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-11-15

Technical Story: a report said that a changed room picture did not show.
Persephone showed no room pictures at all: rooms had only an icon, and a
change gave only the timeline line "X changed the room picture".

## Context and Problem Statement

A room picture (`m.room.avatar`) is an image that another member chose. It is
hostile input, like a received image. Fetching it tells the homeserver, and
through federation the server that holds the file, that this account looked
at the room. The security design gives avatars the same media isolation as
images, and the development plan says that images, avatars and thumbnails
load only on request by default. ADR-0009 covers images in messages, not room
pictures. How does Persephone show room pictures without a new decoder, a raw
fetch from the web view, or a fetch the user did not choose?

## Decision Drivers

* Decode only in the contained WebAssembly guest (ADR-0003). The web view
  never fetches an `mxc` or `http` URL, and never decodes an image.
* Load nothing the user did not choose to load.
* Keep a room's picture current when it changes.
* Keep downloads, decodes and the page's memory bounded.

## Considered Options

* Load each room picture on request, and every listed room's picture when the
  user turns that on (off by default).
* Load every room picture automatically.
* Show no room pictures.

## Decision Outcome

Chosen option: "on request, or automatically when turned on", because it is
the only option that shows pictures and keeps the plan's default of loading
media only on request.

* The room list names each room's current picture by an opaque ID
  (`pictureId`, 16 hex characters hashed from the `mxc` URI) or none. The ID
  changes when the picture changes; no URL reaches the page.
* `load_room_picture(room_id)` takes only a joined room's ID. The core reads
  the room's current picture from the SDK and downloads the server's
  thumbnail (96 × 96, cropped, not animated) through the authenticated media
  API, or the legacy one when that is all the server offers. When the server
  has no thumbnail or refuses one, the core downloads the file itself. Both
  use the image download: at most 20 MiB, checked before allocation, with the
  lock checks of a lease. The contained worker decodes it, and the core
  scales it to fit 96 × 96 and keeps it under an opaque handle. The page reads
  it in bands, as for an image, and releases the handle.
* Pictures have their own admission count (one at a time), so they never take
  an image's slot. They share the one worker slot.
* The account setting `loadRoomPictures` is off by default. Off, the room
  header shows a "Load the room picture" button. On, the page asks for the
  picture of each listed room, one at a time. The page keeps at most 200
  pictures and retries a failed automatic load at most twice.

### Positive Consequences

* Room pictures show, and a changed picture shows once it is loaded again.
* No new decoder, endpoint for the page, or web view capability: the page
  names a room, and receives pixels.
* A thumbnail keeps downloads and decodes small. Some servers make a PNG or
  JPEG thumbnail of a WebP or GIF file, which then can show; the decoder still
  accepts PNG and JPEG only.

### Negative Consequences

* The setting is a policy of the page. The core cannot tell an automatic
  request from a click. A compromised page could load every picture, as it
  could load every image; it gains no other authority.
* Loading discloses interest in the room to the homeserver and the server that
  holds the picture. Turning on automatic loading does this for every listed
  room at once. The setting's text says so.
* Each picture needs one worker run. With automatic loading this is up to one
  run per listed room after each start.
* The thumbnail is made by the server, so a malicious server chooses its
  bytes. It is decoded in the same contained worker as any image.
* For a direct room without a picture of its own, a Sliding Sync server can
  name the other person's picture, which then shows as the room's.

## Links

* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)
* [ADR-0009](0009-strip-sent-image-metadata-and-load-received-images-on-request.md)
* [Image messaging design](../architecture/image-messaging.md#room-pictures)
* [Security design](../architecture/security-design.md)
