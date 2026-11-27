# Tell call members when you are deafened

* Status: proposed
* Deciders: project owner (this record needs the owner's approval of the choices below)
* Date: 2026-11-27

Technical Story: the owner asked that, when a person is deafened, the call
window of every call member shows a muted headset icon in addition to the
muted microphone icon for that person. A person who is only muted shows just
the muted microphone icon.

## Context and Problem Statement

A call member can see who is muted, because the worker reads the LiveKit
track state (ADR-0059). Deafen (ADR-0055) is local: the worker turns off the
remote audio tracks and mutes the microphone. No other member can know that
this person hears nobody. How does a member tell the others that it is
deafened, with the transports that the core and the worker have today?

## Decision Drivers

* The call worker and the call wire protocol stay as they are (ADR-0043).
* The state must reach members on any SFU, and must not need a LiveKit
  permission that the homeserver's token service does not give.
* The message must be encrypted and authenticated like the frame keys
  (ADR-0005), so a member cannot claim the state of another.
* Bounded input, bounded state, and no peer text in the page (ADR-0004).
* No change to Matrix state events that other clients read.

## Considered Options

* LiveKit participant attributes
* LiveKit data messages
* A field in the MatrixRTC member state event
* An Olm-encrypted to-device message, sent by the core

## Decision Outcome

Chosen option: "An Olm-encrypted to-device message, sent by the core",
because it needs no worker change, uses a path that already has the checks
that we need, and does not touch the member state event.

* Event type: `io.github.omg-software.persephone.call.deafened`. The
  content has `room_id`, `member_id`, `member.id` and
  `member.claimed_device_id` as the frame key event has them, and
  `deafened` (boolean) and `seq` (unsigned integer).
* `seq` is a counter. It starts at the milliseconds since the Unix epoch
  when the call starts, and the sender adds 1 on each change. In one run of
  the app every counter is above all the ones given before, so a call that
  starts in the same milliseconds as an earlier one, or rejoins after many
  changes, starts above the earlier call's last counter. A receiver
  ignores a message whose `seq` is not higher than the last one it took
  from that sender device, so a late message cannot undo a newer one. The
  counter is not stored between runs of the app: a new run starts from the
  device's clock. A receiver's note is the last counter that it accepted
  from that device, and it takes a new run's messages only when their
  counter is above that note. The note is at most the start counter of its
  call plus that call's changes (less when a send failed); that start is
  the clock then, or more after quick rejoins in that run. A new run's
  first counter is its clock at its call's start. After that, the counter
  goes up by 1 on each change and does not read the clock; a later call of
  that run starts from the clock at its start, or above the run's last
  counter. So the receiver ignores each message of the new run whose
  counter is not above the note, and takes the first one whose counter is
  above it: after enough changes, or in a later call that starts after the
  clock passed the note. The clock alone does not end it. The note can be
  ahead of its own call's clock: by that call's changes, and by quick
  rejoins in that run, whose start counters go above the clock. So a
  restart soon after the earlier call, or a clock set back, can cause this;
  whether it does depends on the note. The receiver then shows the old
  state until a delivered counter is above the note (see the negative
  consequences).
  The counter shows members roughly when the state changed, which they can
  already see from the call.
* The core sends the message to the devices that get the frame key, with the
  same checks: only devices that their owner signed, and no user whose
  identity changed without approval (`encryption_keys::send_event`).
* The core sends it after the worker answers `Deafened` and the state
  differs from the one last shared. It also sends the current state, deafened
  or not, to each member device that joins, and to the members that are
  already there when this device starts the call. The call loop only queues the
  hint: a sender task of the call sends it, so a slow homeserver never delays
  the call's commands. The task merges waiting hints (the newest content, the
  recipients together, at most 32), and the call's end joins it. A device that
  leaves the call before its send starts is taken out of the waiting
  recipients, so it gets no state chosen after it left. The cap drops
  recipients on purpose: when 32 devices already wait, a device that joins
  before that send starts is left out, and it shows the state it knew (none,
  for a new member) until the state changes again. A failed send is not fatal
  and is not retried: its recipients also get the state only at the next
  change (a new member gets it when it joins). Both cases only leave an icon
  stale.
* The core takes a message only when it is Olm-encrypted, from a device that
  its owner signed, with an unchanged identity, with content that names that
  device, and from a current member of the running call. A message that
  arrives before the membership is parked, like an early key. The content is
  bounded (4 KiB after the 8 KiB event bound). The longest user and room IDs
  fit (255 bytes each); a sender whose content would be larger, through a very
  long device ID, sends none, so no receiver refuses it.
* The core keeps one note for each current member device, at most 32. A
  member that leaves is forgotten, and all notes end with the call. A change
  of the shown state updates the call status. `CallParticipant.deafened`
  carries it to the page as a hint.
* The page shows a headphones-off icon after the microphone-off icon, in the
  call window tiles, the roster and the call bar. Deafen implies mute here
  (ADR-0055), so a deafened member shows both icons.

### Positive Consequences

* No worker, wire protocol, SFU or homeserver change.
* The homeserver sees only an encrypted to-device message of a known size
  class, with no readable content.
* The checks and the recipient set are the ones that frame keys already use.

### Negative Consequences

* The state is the sender's own word. The page shows it as a hint only.
* Only Persephone members show it. Other MatrixRTC clients ignore the unknown
  event type, and their members never show a headset icon in this client.
* Members learn the state of a person who deafens. Members already see the
  microphone state, and the owner asked for this display.
* One more to-device message for each change, and one for each member
  device at the start of the call and at each join, also when not deafened.
  This replaces a note that a receiver kept when it did not read a leave.
* The counter is a clock reading, so it shows roughly when the state
  changed. In one run the counter only rises. Between two runs, a clock
  that is set right after it was ahead is a clock set back. A receiver that
  kept a note from the run whose clock was ahead ignores the later run
  until a delivered counter is above that note (see the next point). When
  the clock was far ahead, the changes of a call do not reach the note, so
  this can last about as long as the clock was ahead, unless the receiver
  forgets the note first: when the member leaves or the call ends.
* No counter is stored between runs. A receiver ignores a new run's
  messages, and shows the old state, while their counter is not above its
  note: the last counter that it accepted from that device. A new run's
  first counter is its clock at its call's start, and each change adds 1
  without reading the clock. So the old state stays until a message
  arrives whose counter is above the note: after enough changes, or in a
  later call that starts after the clock passed the note. The note can be
  ahead of its call's clock by that call's changes and by quick rejoins,
  so a restart soon after that call, or a clock set back, is at risk.
* Not verified against a live homeserver or a second client yet.

## Pros and Cons of the Options

### LiveKit participant attributes

* Good, because LiveKit shows them to all participants.
* Bad, because setting them needs the `canUpdateOwnMetadata` grant, and the
  tokens of the lk-jwt-service do not give it.
* Bad, because the worker and its wire protocol must change.

### LiveKit data messages

* Good, because they reach all participants on one SFU.
* Bad, because livekit 0.11 encrypts a data message with the latest key
  index, which follows the last key set for any participant, so a message
  often fails to decrypt.
* Bad, because the worker and its wire protocol must change, and a member on
  another SFU does not get it.

### A field in the MatrixRTC member state event

* Good, because it is one place that shows who is in the call.
* Bad, because the event is public to the room's members and to other
  clients, which must ignore a field they do not know, and a change needs
  a new state event and a membership refresh for each toggle.
* Bad, because it is not encrypted.

### An Olm-encrypted to-device message, sent by the core

* Good, because it is encrypted, authenticated, and sent with existing code.
* Good, because no other component changes.
* Bad, because it is sent to each device and needs the same recipient checks.

## Links

* Refines [ADR-0055](0055-add-deafen-and-a-call-volume-to-calls.md)
* Uses the key path of [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
  and the checks of [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
