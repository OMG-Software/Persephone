# Write a diagnostic log only when the user turns it on

* Status: proposed
* Deciders: project owner
* Date: 2026-11-23

Technical Story: on 22 November 2026 a call between Persephone (on a
Merovingian homeserver) and Element Web (on matrix.org) had no sound in
either direction. Element's console log explained one direction. Nothing
explained the other: the call worker writes nothing, and the core ignores
LiveKit's frame decryption states. On 23 November 2026 the owner asked for a
diagnostic log of the whole application, off at every start, turned on in
Settings after a warning, as one `.jsonl` file that is deleted at the next
start. The owner chose the three options marked "owner's choice" below.

## Context and Problem Statement

Persephone writes no log (`PRIVACY.md`). The security design says that logs
must exclude secrets and message bodies, and the development plan allows only
allowlisted fields and no stable room or user IDs. With no log, a fault that
the user cannot see, such as a call that connects but plays nothing, cannot be
diagnosed. How does Persephone record enough to diagnose such faults, without
a log that holds secrets, and without a log that stays on disk?

Facts from the source on 23 November 2026:

* No process installs a `tracing` subscriber or a `log` logger. The Matrix SDK
  emits `tracing` events. LiveKit and libwebrtc (through the patched
  `libwebrtc` log sink) emit `log` records. All of them are dropped.
* The call worker writes nothing to stderr on purpose (ADR-0043). LiveKit
  reports frame decryption states (`RoomEvent::E2eeStateChanged`), and the
  worker ignores them.
* Third-party log text is free text. Element Call's own log showed an OpenID
  access token and a LiveKit JWT in plain text. A Rust library can do the same.
* Overwriting a file does not reliably remove its old content on SSDs and on
  copy-on-write or journaling file systems. The security design already says
  not to promise this.

## Decision Drivers

* Diagnose faults in every process: core, UI host, page and call worker.
* Never write a password, access token, refresh token, JWT, OpenID token,
  recovery key, passphrase or frame key.
* Off unless the user turns it on in this run, after a warning shown in the
  core's own window, which the page cannot fake.
* Bounded memory, disk and work, also when a process or the page misbehaves.
* Keep the web view and the UI host keyless (ADR-0004), and keep the call
  worker's authority (ADR-0043).

## Considered Options

* One log written by the core, fed by every process, with redaction
* A log file for each process
* No log; ask the user for screenshots and other clients' logs

## Decision Outcome

Chosen option: "One log written by the core, fed by every process, with
redaction", because only the core may write to the profile, and one writer
can apply one redaction rule, one size limit and one deletion rule.

### Switch and warning

* Diagnostic logging is off when the core starts. The setting is not stored.
* Settings has a Diagnostics panel with a "Turn on diagnostic log" button.
  To turn it on, the page sends `set_diagnostics { on: true }`. The core then asks in its own window
  (the same native yes/no window as for links, ADR-0024). Only "yes" turns it
  on. The window says that the log can hold account names, user IDs, room
  IDs, server names, device IDs, IP addresses and the times and types of
  events; that it never holds passwords, tokens or keys on purpose; that
  third-party text is filtered but the filter cannot be complete; and that
  the file is deleted at the next start of this profile. "No" is the
  default answer. While
  one question is open, a second request is refused with `limit_exceeded`.
* Turning it off needs no question. The file stays until the next start, so
  the user can send it (owner's choice).
* When the core ends, also for a profile switch, it turns the log off after
  the stores lock, so the queue is written and the file is synced. A switch
  starts the other profile's core as a new process, after this one's log
  is off; that core has the other profile's file, and its own start erases
  that file.
* The core announces each change with the content-free event
  `diagnostics_changed { on }`. The UI host and the page start or stop
  sending their records.
* The `--demo` mode has no profile and no log.

### The file

* One file, `diagnostics/persephone.jsonl`, in the profile's directory.
  Settings shows its full path (`diagnostics_status`), so that the user can
  find the file. This is the one exception to ADR-0015's rule that the UI
  sees slot numbers only: the path goes out to the page only, and no command
  takes a path from the page. One JSON object for each line.
* At start, before the profile opens, the core deletes the file if it exists:
  it overwrites the whole length with random bytes, flushes to disk, truncates
  and removes the file (owner's choice). Settings and `PRIVACY.md` say that
  this is best effort: an SSD or a copy-on-write file system can keep old
  blocks that no application can reach. Each profile has its own file, and
  only the started profile's file is erased: a profile that is not started
  again keeps its file until it is started. `PRIVACY.md`, the
  warning and Settings say so.
* The file is opened to write at its end, without following links, and on
  Unix with mode `0600`. The directory is created with mode `0700` on Unix.
  The core is its only writer. Before writing, the core removes an
  unfinished last line that a failed write left, back to the last newline,
  so every line stays one JSON object; a tail with no newline in 64 KiB is
  refused, because no line is that long.
* The file is at most 256 MiB. At the limit the core writes one
  `log_full` record and stops writing until the next start. The switch stays
  on; `diagnostics_status` reports `full`, and Settings says so.
* A failed write, flush or sync (a full disk, a lost drive) stops the
  writing too, because the file's end is then unknown. `diagnostics_status`
  reports `failed`, Settings says that the file may end early, and no more
  records are built. Turning the log off and on again opens the file again
  and clears `failed`.

### Records

Each line has `ts` (Unix milliseconds), `seq` (a counter for this run),
`source` (`core`, `ui`, `page` or `call_worker`), `level`, `target`,
`message`, and optional `fields`.

* Every `tracing` event and every `log` record of the core at `debug` level
  or higher, from Persephone and from every library, except the transport
  crates, which are too busy at `debug`: `h2`, `hyper`, `hyper_util`, `mio`,
  `reqwest`, `rustls`, `tokio`, `tokio_util`, `tungstenite`,
  `tokio_tungstenite` and `want` are kept only from `info`. `trace` is never
  kept.
* Every Matrix event that the SDK hands to the client (timeline, state,
  stripped state, ephemeral, account data and to-device), as metadata only:
  kind, room ID, event ID, type, sender, state key, server time and content
  size. For an encrypted event that was decrypted, also that it was
  decrypted and the sender's device. The type is the one sent
  (`m.room.encrypted`); the decrypted type is not kept.
  The content is never written (owner's choice). Message text, media
  information, frame keys and room keys are content. This covers the core's
  own event records only: a library's own `tracing` or `log` text is
  filtered for credentials, not for content. `PRIVACY.md`, Settings and the
  warning say so.
* Every request from the UI: the command name, the outcome or error code and
  the duration. Never the arguments. Successful `call_video_frame`,
  `image_band` and `diagnostic_records` requests are not noted, as on the
  page: the first two run many times each second, and the last only feeds
  the log.
* The UI host's `log` records: its own at `debug`, other crates at `info`.
* The page's records: the name, outcome code and duration of each bridge
  call, and uncaught errors and rejections. Never arguments or replies. A
  failed call or a rejection is kept only as a short lowercase code, and
  anything else, an `Error`'s message included, as `error`. An uncaught
  error keeps its message and place, which the filter removes credentials
  from. Successful `call_video_frame`, `image_band` and
  `diagnostic_records` calls are not noted: the first two run many times
  each second, and the last only feeds the log.
* The call worker's `log` records (LiveKit and libwebrtc) at `debug`, the
  transport crates under them only from `info` (the same list as the
  core's, above: one constant, `QUIET_LOG_TARGETS` in the call protocol
  crate, used by both), and one
  record for each LiveKit room event that matters for media: participant,
  track published, subscribed, unsubscribed and subscription failed, frame
  decryption state, connection state and quality. Identities and track IDs
  are included. Payloads are not. A token refresh is noted without the
  token.
* The call worker's records are free text from untrusted libraries and the
  SFU. This is the one exception to the call protocol rule "never add free
  text": the core only writes them to the log, through the same filter, and
  never shows them or acts on them. The worker accepts `SetDiagnostics`
  before `Join` and sends records only while it is on.

### Redaction

Every string goes through one filter in the core before it is written,
including field names and values from other processes:

* A field whose name contains `password`, `passphrase`, `passwd`, `secret`,
  `token`, `jwt`, `credential`, `authorization`, `cookie`, `recovery` or
  `private`, or has a part (split at `_`, `-`, `.` or a space) that ends
  with `key` or `keys` (`key`, `frame_key`, `apikey`, but not `keyed`), is
  written as `[redacted]`. The case does not matter. The one exception is
  `state_key`, which is kept: it is event metadata (see Records), not a
  credential. An exception is added only for a named metadata field, never
  for a pattern.
* In free text, these are replaced with `[redacted]`: JWTs (`eyJ…` with two
  dots), Matrix tokens (`syt_`, `syr_`, `mat_`, `mct_`), the value after
  `access_token=`, `token=`, `password=` and `Bearer `, JSON string values of
  members with the names above, and recovery keys (groups of four base58
  characters that start with `Es`).
* Strings are cut to 4 KiB. A record is cut to 16 KiB. Text that any
  process cuts ends with `…`, so a cut record can be told from a whole
  one; in the UI host and the call worker the `…` counts against the
  protocol's limit.
* The core never formats a secret type into a record. Secret holders keep
  `Debug` implementations that hide their content.

The filter cannot be complete for free text from libraries. The warning says
so.

### Bounds

* Records go into a bounded queue of 4096 lines, written by one thread. A full
  queue drops records and counts them, before they are redacted or built.
  The next written line reports the count; when no line follows, the writer
  writes the count before it stops.
* While the log is off, each process keeps the `log` crate's level at
  `Off`, so a library's `log::debug!` costs one atomic read and never
  reaches the logger. Turning the log on sets `Debug`; turning it off sets
  `Off` again.
* An "off" waits for the writer, so it runs off the serving loop, but only
  when the log is on: an "off" for a log that is off is answered at once.
  One drain runs at a time, and a second "off" meanwhile is refused before
  a thread is started. Every "off" cancels an "on" that waits for the
  user. So drain threads, also those whose reply waits for a UI that does
  not read, are bounded by the user's "yes" answers.
* The core itself admits at most 256 records each second from each other
  process (UI host, page, call worker), whatever that process promises. The
  rest are counted the same way, so a flood from one process cannot fill the
  queue or the file.
* The UI host and the page send at most 64 records for each call, each
  message at most 1 KiB and each target at most 64 bytes, and at most
  96 KiB of records as JSON (escaping can make a control character six
  bytes, and the control frame is 128 KiB). The UI host and the core refuse
  more. The page keeps at most 256 records and sends at most one batch each
  second. The UI host keeps at most 1024 records and waits 250 ms between
  batches, so it stays within the core's 256 records each second. Both
  count what they drop, also a batch that the host or the core refused,
  and report the count.
* The call worker keeps at most 256 records, sends at most 200 records each
  second and drops the rest, with a count. LiveKit's and libwebrtc's `log`
  records take at most 192 of the waiting places and 150 of each second's
  records. The rest stays for the worker's own room event notes. A log of
  23 November 2026 showed why: libwebrtc wrote about a thousand ICE records
  at each connect, and the notes of the tracks and their frame encryption
  state, which came at the same time, were dropped. A message is at most 2 KiB and a
  target at most 64 bytes. Each writer round sends the waiting control
  messages, then a waiting picture, then at most 8 records, so records
  never go before a control message or a picture that waits, and a
  steady stream of pictures does not stop them. A record that the full
  queue refuses does not use the second's budget. The call protocol
  version is 8.
* Amendment, 2026-11-25: the call worker also notes the audio, as `Note`
  records of the target `persephone::call_worker::audio`, with counts, flags
  and levels only: no device ID or name, no key and no token. At the join:
  how many microphones and speakers libwebrtc lists, how each was chosen and
  whether the system default was read. At each microphone setting
  (ADR-0062): the gain and the automatic level asked, and what the audio
  processing module reports for echo cancellation, noise suppression and
  automatic gain control. Every 5 seconds: the own level, the playout state
  and volume, the audio device module's state (playout prepared and running,
  capture running), and for each remote audio track (at most 8) whether it is
  enabled, the packets received and lost, the samples decoded and concealed
  and the level. A call with no sound is then told apart by these counts:
  packets that do not arrive, packets that arrive and do not decode, and
  sound that plays at level 0.

### Positive Consequences

* Faults that the user cannot see can be diagnosed from one file.
* The log exists only after a deliberate choice in this run, and it is
  removed at the next start.

### Negative Consequences

* While on, the log holds IDs, server names, device IDs, network addresses
  and the timing of events. This replaces, for this file only, the rule in
  the development plan that forbids stable room and user IDs in diagnostics.
* Redaction of third-party free text is a filter, not a proof.
* Deletion is best effort on SSDs and copy-on-write file systems.
* Writing every event costs CPU and disk while the log is on.
* Not verified live.

## Pros and Cons of the Options

### One log written by the core, fed by every process, with redaction

* Good, because one place applies redaction, limits and deletion.
* Good, because contained processes get no new file access.
* Bad, because other processes need new protocol messages.

### A log file for each process

* Good, because no protocol changes.
* Bad, because the contained call worker and the web view would need file
  access, which breaks ADR-0004 and ADR-0043.
* Bad, because four places must redact and delete.

### No log

* Good, because nothing is written.
* Bad, because faults like the call on 22 November 2026 cannot be found.

## Links

* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0024](0024-open-links-only-after-the-core-asks-in-its-own-window.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [Security design](../architecture/security-design.md)
* [Development plan](../architecture/development-plan.md)
