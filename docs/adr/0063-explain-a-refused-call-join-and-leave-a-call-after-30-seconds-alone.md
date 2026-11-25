# Explain a refused call join, let moderators allow it, and leave a call after 30 seconds alone

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-11-25

Technical Story: a member without the power level for call memberships who
joined an existing call by a path other than the room's call buttons got no
clear message, or only "Your homeserver refused the call membership." The
owner asked whether a moderator can allow members to join calls, and asked
that a call end when this account is alone in it.

## Context and Problem Statement

A MatrixRTC join writes the state event `org.matrix.msc3401.call.member`
(ADR-0043). A room's power levels decide who may send it. In a room with the
Matrix defaults only people at 50 or more may, so an ordinary member cannot
join a call there, although a call may already run.

`calls/status.rs` `readiness` already answers `NotPermitted` from the local
power levels, and the room's call buttons then show a note on click. The core
did not check the level itself when it joined. The join went on to the SFU
discovery, the worker launch and the delayed event, and the homeserver's
refusal of the membership came back as the generic `membership` error. A
join through an incoming ring's answer button reaches the core the same way,
and the ring's room is often not the room whose readiness the page holds.

Two more problems follow. A moderator has no way in Persephone to set the
level of this one event type. A call also stays open when everyone else has
gone, and keeps the microphone and the membership until the user leaves.

Facts from the source on 25 November 2026:

* The page joins only through `call_join`, from the room header's two call
  buttons (`startCall`) and from a ring's answer buttons (`answerRing`).
  Both go to `runJoin`, which shows the `joinError` text. No other control
  joins: the call bar, the call window and the room timeline only show a
  call that this account is in.
* In `CallController::join`, `prepare` reads the room, checks that it is
  joined and encrypted, discovers the SFU, starts the worker, schedules the
  delayed leave, and only then sends the membership.
* The worker sends `ParticipantJoined` and `ParticipantLeft` for every remote
  participant, with the number of the SFU connection (0 is the own SFU, 1 to
  3 are other SFUs, ADR-0054), and `Connected` when it is joined.
* ADR-0017 sets the room's levels through one closed change call. Event types
  in `events` are named by `LevelKey`; `room_settings::state_type` maps a key
  to its state event type.

## Decision Drivers

* Say clearly why a join is refused, and say what fixes it.
* Make no request, delayed event or worker for a join that the room's levels
  already forbid.
* Add no UI capability beyond a closed code and a closed level key.
* Bound the new timer, and prefer staying in a call to leaving it wrongly.

## Considered Options

* A closed `not_permitted` join error, a core check before any action, and a
  `join_calls` level key; the alone rule in the call task
* Show the permission text only in the room header
* Let the page check the levels and join anyway when it thinks it may

## Decision Outcome

Chosen option: "A closed `not_permitted` join error, a core check before any
action, and a `join_calls` level key; the alone rule in the call task",
because the core is the only process that holds the room state and the
session, and every join passes through it.

### Refused join

* `CallJoinError::NotPermitted` (`not_permitted`) is new. `CallError::NotPermitted`
  maps to it in `calls/status.rs`.
* `prepare` reads the room's power levels from the local store, after the
  joined and encrypted checks and before the SFU discovery, the TURN request,
  the worker launch, the delayed event and the membership send. When
  `can_send_member_state` is false, the join ends with `NotPermitted`, and no
  such action ran. The readiness answer uses the same function, so the two
  cannot differ.
* The local levels may be stale. When the homeserver answers the membership
  send with status 403 and `M_FORBIDDEN`, the join ends with `NotPermitted`
  too. A 403 with another code, any other 4xx and any 5xx stay `membership`.
  The cleanup for a refused send is unchanged: a client error means that
  nothing was written, so the delayed event is cancelled.
* The page shows one text wherever a join is blocked or refused: "You do not
  have permission to join calls in this room. A room moderator can allow it
  in Room settings, Permissions, Join calls." The room header's two buttons
  already look disabled (`aria-disabled`) and show the reason on click from
  the room's readiness; `not_permitted` now has this text. A refused join of
  any other path shows it as the join error.
* A ring's answer buttons stay enabled. Readiness is read only for the room
  that is open, and a ring may come from another room. The core's refusal
  gives the same text after the click. Reading readiness for each ring is a
  possible follow-up.
* The proxy question of ADR-0043 still comes before `join`, so a profile with
  a proxy can show it before a refusal.

### Moderators allow joining

* `LevelKey::JoinCalls` (`join_calls`) is new. `room_settings::state_type`
  maps it to the state event type `org.matrix.msc3401.call.member`, so it is
  read from, and written to, the `events` map of the power levels like the
  other state keys. The change stays one `RoomChange::Level` call with the
  validation of ADR-0017: a level above the account's own, an unknown key
  and a level outside the Matrix range are refused.
* Room settings, Permissions shows it as "Join calls", in its own "Calls"
  group. A moderator picks "Member" (level 0) to let every member join. The
  level is the level to send this one event type; the level needed to send
  other state events does not change.
* The defaults for new rooms (ADR-0016) do not change in this record. A
  follow-up could set this level to 0 in the rooms Persephone creates, so
  members can join calls there by default. That choice changes who can
  publish a membership state event and needs the owner's decision.

### Leave a call after 30 seconds alone

* `calls/alone.rs` keeps the set of remote participants by connection number
  and identity, from `ParticipantJoined` and `ParticipantLeft`, and one
  deadline. `Connection { up: false }` removes every participant of that
  connection, because another SFU that ends sends no leave for each of them. The deadline is set when the worker has sent `Connected` and the
  set is empty, so a call that is still connecting does not count. It is
  cleared when anyone joins on any connection, and set again to 30 seconds
  from the moment the last one leaves.
* The call task waits for the deadline beside its other events. When it
  passes, the task ends with `EndReason::Alone`, which is the path of a
  normal end: the worker gets `Leave`, the membership is removed (or the
  delayed event is sent), and the handlers go. The page sees the call end
  with the code `alone` and the text "You were the only person in the call
  for 30 seconds, so the call ended."
* The timer belongs to one call task. A leave by the user, a lock, a sign-out,
  a worker failure and the end of the session end the task and drop the
  timer. A new call starts with a new timer.
* The set holds at most 4 times `MAX_PARTICIPANTS` entries. A call that goes
  over the bound is never ended for being alone. A participant that the
  worker never reports as gone keeps the call. A second device of this
  account in the call counts as another participant. Presence is the SFU's
  word, so an SFU that hides a participant can end a call after 30 seconds;
  that is a loss of the call, not of confidentiality.

### Positive Consequences

* A refused join gives one clear message with the way to fix it.
* A refused join does no network work and starts no worker.
* Moderators can open calls to members from the settings.
* An empty call ends by itself and stops using the microphone and the
  membership.

### Negative Consequences

* The ring's answer button does not show the refusal before the click.
* The 30 second limit is fixed. A user who waits for others alone in a call
  must stay within it.
* A stale local power level can still let a join reach the homeserver, which
  then answers 403; the second rule covers that case.

## Pros and Cons of the Options

### A closed `not_permitted` join error, a core check before any action, and a `join_calls` level key; the alone rule in the call task

* Good, because every path shows the same text from one core decision
* Good, because the check uses the same function as readiness
* Good, because the timer is pure and testable with paused time
* Bad, because one more closed code goes through contracts and the page

### Show the permission text only in the room header

* Good, because it changes less code
* Bad, because a ring still shows the generic error

### Let the page check the levels and join anyway when it thinks it may

* Bad, because the page's view of the levels is a hint, not a decision
* Bad, because it weakens the rule that the core checks every join

## Links

* Refines [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md), [ADR-0052](0052-ring-for-incoming-calls-and-show-active-calls.md) and [ADR-0054](0054-connect-to-call-members-on-other-sfus.md)
* Uses [ADR-0017](0017-configure-joined-rooms-through-one-closed-change-call.md); related to [ADR-0016](0016-create-encrypted-rooms-and-answer-every-invitation.md)
