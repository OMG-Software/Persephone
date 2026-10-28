# Send and save files through the core's own dialogs

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for sending and saving
files other than images.

## Context and Problem Statement

Persephone sent and showed only images (ADR-0009); other files arrived as
"a kind of message Persephone does not show yet". Files carry the same
risks as images and more: their names can hide what they are, and opening
one runs whatever handles its type. The web view must not name paths or
reach the file system (ADR-0004). How are files chosen, sent, received and
saved?

## Decision Drivers

* No path, and no file system access, in the web view.
* Nothing received is opened, previewed, played or decoded.
* Bounded memory; encrypted rooms send encrypted files.
* Reuse the image paths where they already hold.

## Considered Options

* Let the page read files with a Tauri file-system plugin.
* Choose, read, download and write files in the core, through its own
  dialogs, as for images and the recovery key.

## Decision Outcome

Proposed option: "in the core, through its own dialogs".

* "Attach a file" opens the core's file dialog ("Choose a file to send to
  this room"); the choice is the user's decision to send, so the file is
  read and queued at once. It is read as a picked image is: no link or
  junction, no directory, not empty, at most 6 MiB, shared for reading
  only. It counts toward the one unsent attachment the outbox allows, and
  is retried, discarded and asked consent for like any message.
* It is sent with its file name, made safe (separators, reserved and
  control characters replaced, no Windows device name, at most 120
  characters), as `application/octet-stream`, so no receiver is told to
  treat it as anything else. The SDK encrypts it in an encrypted room.
* Received `m.file`, `m.audio` and `m.video` events are `file` items: the
  safe name, the declared size, and whether the file is encrypted. Nothing
  is fetched.
* "Save…" asks the core: it downloads and decrypts the file like an image
  (at most 20 MiB, checked by its hash), then asks where to save in its own
  Save dialog, suggesting the safe name, and writes it: to a new file
  beside it first, which then replaces the destination in one rename, so a
  failed save leaves a file already there unchanged. Nothing opens it.

### Positive Consequences

* Files work without giving the web view any file or path authority.
* A received file reaches the disk only where the user chose, and never
  runs.

### Negative Consequences

* Audio and video are saved, not played.
* Files sent are at most 6 MiB, as are images after stripping. At its
  peak a send holds up to six copies in matrix-sdk 0.19.1: the outbox's,
  the SDK's input, its upload buffer (a clone, or the encrypted copy with
  spare capacity), the clone of the request it sends and the HTTP body.
  All are reserved in the 40 MiB encoded budget. Saved files keep the
  20 MiB limit.
* The sent name can say something about the file; it is the one the user
  chose.
* Windows only, like the image dialogs; other platforms fail closed.
* Tested with unit tests of names, file events and the outbox, and IPC
  tests; the dialogs and a live send are not exercised by an automated
  test.

## Links

* Extends [ADR-0009](0009-strip-sent-image-metadata-and-load-received-images-on-request.md)'s rules for picked and received media.
