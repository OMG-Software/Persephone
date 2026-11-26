# Show call statistics on request

* Status: proposed
* Deciders: project owner
* Date: 2026-11-25

Technical Story: on 25 November 2026 the owner asked for a "Technical
details" button in the call window. It shows the numbers behind a call: for
the own microphone, camera and screen share, for each other participant, and
for the connection to each call server. The owner wants to see why a call is
silent, choppy or slow.

## Context and Problem Statement

The call worker runs the call inside its containment (ADR-0043). Only the
worker can read WebRTC's statistics, and the web view has no connection to
the call at all. ADR-0059 already reads the sound levels of each track from
`get_stats` every 100 ms for the meters. The new details need many more
numbers from more tracks. How does Persephone show them without a new way for
the worker's or the SFU's data to reach the page, and without any cost while
nobody looks?

Facts from the source on 25 November 2026:

* LiveKit 0.11.0 gives `get_stats` for each local and remote track, as typed
  values (`RtcStats`). The report of a track includes its transport and the
  selected candidate pair. `Room::get_stats` also exists, but a `Room` is
  not clonable into a task.
* The statistics hold addresses, ports, candidate types, SSRCs and codec
  names. The details need none of them.
* Counters (bytes, packets) only grow. A rate is the change between two
  readings. A counter falls when WebRTC restarts a stream.

## Decision Drivers

* No work, and no new message, while the panel is closed.
* The worker sends numbers only, with bounds that the receiver checks
  (ADR-0043). No free text, address or identifier beyond those that other
  messages already carry.
* In each round of the worker's writer, statistics go after the control
  messages and the waiting picture, and they never run in the worker's event
  loop. A control message that comes while a reading is being written waits
  for that one bounded write.
* The call window gets two new commands and nothing else (ADR-0060):
  `call_stats`, the switch that reaches the core, and `call_stats_read`,
  which only tells the host that the page handled a reading.

## Considered Options

* On request: the page turns the details on and off, and the worker reads
  once a second only while they are on
* Always on: the worker reads every second during every call and the core
  keeps the newest reading
* The page asks for each reading (a pull command)

## Decision Outcome

Chosen option: "On request", because it costs nothing while closed, and only
one of its two commands, the switch, reaches the core. The other,
`call_stats_read`, stays in the UI host and only paces the page. With a pull
command, each reading is a request that reaches the core and the worker, and
the worker must read for each request or keep the newest reading always.

### Switch

* The call window sends `call_stats { on, page }` with a channel. The core
  command is `Call::CallStats { on }`. Turning the details on needs a running
  or joining call (`unavailable` otherwise); turning them off always works,
  also while the session is locked or busy, so a closed panel or window never
  leaves them reading.
* The core turns them off when the call ends (in the same step that frees the
  call slot, so a late "on" cannot reach the next call), when the UI
  connection closes, and when the call window that asked reloads or closes.
  The host keeps the channel only while they are on, and tells the core to
  stop when it drops one.
* The core sends `ToWorker::SetStats { on }` to the worker on each change.
  The worker starts with them off. A full worker queue keeps the newest value
  for a retry, like the diagnostic switch (ADR-0058).
* The core sends a change only. When the page turns the details off and on
  again while the call task is busy, the task can see only the newest value,
  "on", and sends nothing: the worker never stopped, so its series and its
  rates go on. When both changes reach the worker, it starts a new series,
  whose first reading has no rates. Readings taken before the off can still
  arrive after the on: the one that the worker's writer holds, and those that
  wait in the core's queue of worker messages (`EVENT_QUEUE`, 64 messages of
  every kind) while the call task is busy. The worker reads at most once a
  second, so at most one reading waits for each second that the task is
  busy. The core passes them on. The core and the UI host keep only the
  newest waiting reading, but the page can still show more than one of them,
  one after the other. Each is a reading of the same call, taken before the
  off, and the next new reading replaces it. A series number in `SetStats`
  and `Stats` would let the core drop them; this record does not add one.

### Worker

* `StatsProbe` (`session/stats_probe.rs`) reads while the switch is on, at
  most once a second, one read at a time, on its own task with a deadline of
  900 ms. It is the same design as `SoundProbe` (ADR-0055, ADR-0059): the
  event loop only starts it and takes its report. It is aborted when the
  call ends. A report of an older on-off series is dropped by its number.
* A read asks for the statistics of the own microphone, camera and screen
  tracks, and of one audio and one video track of each remote participant
  (at most 32 participants, so at most 64 remote reads): the microphone, and
  the camera before a screen share, with the smallest track ID when there
  are more, so the reads follow one track while it stays. Each read keeps
  its track ID and the time it finished.
  libwebrtc gives each track only its own streams and the transport they
  use. The own room uses LiveKit's single peer connection when the server
  offers it; its own and remote tracks then read one transport, which counts
  once. With a publisher and a subscriber connection, both add, each from its
  own counters. The connection numbers come from the transport and the
  selected candidate pair that those reports already include. A connection
  with no track has no entry. When a transport that a connection uses was
  not read (its read missed the deadline), the connection's send and
  receive rates are unknown for that reading, never short.
* `mapping/stats.rs` is pure. `StatsBook` keeps the last sample of each
  counter, with the time of the read that gave it, under the track's ID (or
  the connection's transport). Each rate is the change since that sample,
  over the time between the two reads, so a slow read of another track
  never changes it, and a new track never compares with an old one. A
  counter with no earlier value, and a zero or negative interval, give no
  rate (`null`), never a wrong one. A counter that fell was restarted and
  gives no rate, with one exception: a 32-bit counter that fell by more than
  half its range wrapped, and counts across the wrap (`counter_delta`). A
  64-bit counter (bytes) never wraps. A restart of a 32-bit counter from
  above half its range reads as a wrap; its rate is still clamped. Every
  number is clamped to its bound, and an entry whose identity
  or connection the protocol would refuse is left out.
* The reading goes to `Output::offer_stats`. It never waits, it replaces a
  reading that has not gone out, and the writer writes it after the control
  messages and the waiting picture of its round. So at most one reading
  waits. A control message that comes while the reading is being written
  waits for that write, which the message limits bound.

### Message

`FromWorker::Stats` (tag 23, protocol version 13) holds integers only:

| Part | Content | Bound |
| --- | --- | --- |
| Own microphone | level, bitrate, packets sent, packets lost by the far end, jitter, round-trip time | level 0 to 100, bitrate 0 to 1,000,000 kbit/s, times 0 to 60,000 ms |
| Own camera and screen share | frame rate, size, bitrate, quality limit (none, processor, bandwidth, other), packets lost by the far end | 0 to 120 fps, sides 0 to 16,384 |
| Each remote participant | audio: level, bitrate, packets received and lost, jitter, concealed share. Video: frame rate, size, bitrate, frames dropped each second, packets lost | at most 32; each (connection, identity) once; concealed 0 to 100 |
| Each connection | estimated upload bandwidth, total send rate, total receive rate, round-trip time | at most 4; numbers 0 to 3, each once |

A value that is not known yet (a rate before the second reading, a
round-trip time before the first report) is absent, not zero. The message is
about 18 KiB at most and the protocol limit is 64 KiB. The reader checks every
count before it allocates, refuses a repeated participant or connection, and
the writer leaves out entries past the limits. There is no address, port,
candidate type, SSRC, codec or other text.

### Core and host

* While the details are on, the core names each remote entry by the member
  that the worker identity stands for (`View::level_member`: a current member
  whose connection speaks for it), leaves out any other, and checks the
  result against `CallStats::within_limits`. While they are off, a reading
  is dropped.
* The core keeps only the newest reading and sends it as `Event::CallStats`.
  The UI host reads the event with the same limits and sends it only to the
  channel of a window that turned the details on. The UI host sends each
  page one reading at a time: the next waits until the page says, with
  `call_stats_read`, that it handled the last one, and only the newest waits.
* The UI host keeps a window's channel only after the core accepted the
  start. Its own stop on a reload or close is a request too, so the host
  lets one change of the core's switch run at a time, and each ends with the
  switch as the kept channels want. A start that a reload or close overtook
  keeps no channel and turns the switch off again; a late stop does not
  undo the start of the page that came next. Tauri reads a command's
  arguments only in its task, which may start after the reload or close, so
  the host cannot tell from the time which page sent it. So each window
  counts its pages: the page load and `watch_session` (a page's first
  request) each count a new page, and `watch_session` gives the page its
  count. The page sends the count with each `call_stats`, and the host takes
  a start or stop only with the count of the page the window shows now. A
  request of a page that is gone fails and changes nothing.
* The capability `call` gains `allow-call-stats` (the switch, which reaches
  the core) and `allow-call-stats-read` (the page's acknowledgment, which
  stays in the host and reaches no core), and nothing else. The main
  window has neither.

### Page

* A button "Technical details" (an icon button) in the call window controls
  opens a panel. The panel turns the details on when it opens and off when
  it closes, so closing, a closed window and an ended call all stop them.
  The window closes the panel when the call ends and when a call in another
  room replaces it (`statsStayOpen`), so the panel never shows a reading of
  an earlier call. The call status has no call ID, so a new call in the
  same room is told apart only by the end status between the two.
  Escape closes it. The bridge keeps the page's count from `watch_session`
  and sends it with each start and stop; with none, a start fails and a stop
  asks nothing. It sends the starts and stops one at a time, in the order
  asked: the host keeps one channel per window, so the late stop of
  a closed panel must never reach it after the start of the panel opened
  next. The button is a disclosure (`aria-expanded`, `aria-controls`), like
  the device button.
* The panel shows a table per source with units: kbit/s, fps, ms, %,
  packets. Participants show as in the call window. All text is inert. The
  page checks each reading against the contract again and drops a bad one.

### Positive Consequences

* No cost while the panel is closed: no read, no message, no timer work.
* The numbers cannot carry anything but numbers, and the page never sees an
  address.
* A reading goes after the control messages and the waiting picture of each
  writer round, so it delays a control message by at most one bounded write.

### Negative Consequences

* The reading takes up to a second to appear, and rates show as unknown
  until the second reading.
* Each read starts from LiveKit's copy of each room's participant map
  (`Room::remote_participants`, one identity and two shared handles per
  participant), and the limit of 32 applies after that copy. The level
  meters take the same copy ten times a second. A bounded participant index
  kept from room events would bound both; it is not part of this record.
* An entry whose track has no statistics (a slow or failed read) is missing
  for that second.
* A connection that has no track shows no numbers, because the worker reads
  transports through tracks.
* The numbers are WebRTC's own and were not compared with another tool. They
  were tested with synthetic readings only; no live call was measured for
  this record.

## Pros and Cons of the Options

### On request

* Good, because it costs nothing while closed.
* Good, because it adds one event and two commands, and only one of the
  commands (the switch) reaches the core.
* Bad, because the first reading takes a second.

### Always on

* Good, because the first view is immediate.
* Bad, because every call pays for reads that nobody looks at, on up to
  70 tracks a second.

### Pull command

* Good, because the page paces itself.
* Bad, because each reading is a request that reaches the core and the
  worker, and the worker has to read for each request or keep the newest
  reading always.

## Links

* Refines [ADR-0060](0060-show-calls-in-a-separate-call-window.md): the call
  window's capability gains `call_stats`.
* Follows the probe design of [ADR-0055](0055-add-deafen-and-a-call-volume-to-calls.md)
  and [ADR-0059](0059-test-call-devices-show-each-participants-level-and-switch-devices-during-a-call.md).
* Follows the switch design of [ADR-0058](0058-write-a-diagnostic-log-only-when-the-user-turns-it-on.md).
