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
  when the call starts, and the sender adds 1 on each change. A receiver
  ignores a message whose `seq` is not higher than the last one it took
  from that sender device, so a late message cannot undo a newer one. A new
  call's counter is above any counter of an earlier call of the same
  device, so a note that a receiver kept from an earlier call is replaced.
  The counter shows members roughly when the state changed, which they can
  already see from the call.
* The core sends the message to the devices that get the frame key, with the
  same checks: only devices that their owner signed, and no user whose
  identity changed without approval (`encryption_keys::send_event`).
* The core sends it after the worker answers `Deafened` and the state
  differs from the one last shared. It also sends the current state, deafened
  or not, to each member device that joins, and to the members that are
  already there when this device starts the call. A failed send is not fatal and is not retried;
  the next change or the next new member sends the state again.
* The core takes a message only when it is Olm-encrypted, from a device that
  its owner signed, with an unchanged identity, with content that names that
  device, and from a current member of the running call. A message that
  arrives before the membership is parked, like an early key. The content is
  bounded (1 KiB after the 8 KiB event bound).
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
  changed. A clock set far ahead makes later calls of that device lose
  against it only until the clock is right; a restart of the clock backwards
  between two calls can hide the new state of the later call.
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
