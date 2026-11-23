// The line shown for an event that is neither a message nor a room change,
// which setting shows or hides it, and which timeline items the settings
// show. Pure functions, no host calls.
import type { OtherEvent, TimelineEvents } from '../../crates/contracts/generated/contracts';
import { changeShown, type StatePart } from './stateText.ts';
import type { ShownItem } from './timeline.ts';

/**
 * Fixed text after the sender's name, per kind. `null` marks the kinds the
 * page has no text for: their line names the event's type instead. Every
 * kind is the sender's claim; a verification step says nothing about whom
 * this account trusts.
 */
const OTHER_TEXT: Record<OtherEvent, string | null> = {
  verification_request: ' sent a verification request',
  // Any room member can send these events, so the line is the sender's
  // claim and never says that a verification took place.
  verification_ready: ' says they are ready to verify',
  verification_start: ' says they started verification',
  verification_step: ' sent a verification step',
  verification_cancelled: ' says they cancelled verification',
  verification_done: ' says they finished verification',
  call_started: ' started a call',
  call_answered: ' answered a call',
  call_declined: ' declined a call',
  call_ended: ' ended a call',
  call_signalling: ' sent call connection details',
  call_joined: ' joined the call',
  call_left: ' left the call',
  call_updated: ' updated their call membership',
  poll_started: ' started a poll',
  poll_vote: ' voted in a poll',
  poll_ended: ' ended a poll',
  sticker: ' sent a sticker',
  location: ' shared a location',
  location_stopped: ' stopped sharing their location',
  space_child: ' changed the rooms in this space',
  space_parent: ' changed the spaces this room belongs to',
  widget: " changed the room's widgets",
  third_party_invite: ' invited someone by email or phone',
  message: null,
  event: null,
  state: null
};

/** What a line with no fixed text says before the event's type. */
const TYPE_TEXT: Partial<Record<OtherEvent, string>> = {
  message: ' sent a message of type ',
  state: ' changed room state of type '
};

/**
 * The pieces of the line for an other event from `sender`. The type is a
 * `name` piece: the page shows it in its own <bdi>, as text.
 */
export function otherParts(sender: string, event: OtherEvent, eventType: string): StatePart[] {
  const fixed = Object.hasOwn(OTHER_TEXT, event) ? OTHER_TEXT[event] : null;
  if (typeof fixed === 'string') return [{ person: sender }, { text: fixed }];
  // A kind this build does not know (the core never sends one) is named by
  // its type, like any other event.
  const before = Object.hasOwn(TYPE_TEXT, event) ? TYPE_TEXT[event] : undefined;
  return [
    { person: sender },
    { text: before ?? ' sent an event of type ' },
    { name: eventType }
  ];
}

/**
 * What a deleted event of each kind was, before " deleted.". Only
 * message-like kinds can be deleted items; a kind with no entry is named by
 * its type. The line names no one as the deleter: a moderator may have
 * deleted it.
 */
const DELETED_TEXT: Partial<Record<OtherEvent, string>> = {
  verification_request: 'Verification request',
  verification_ready: 'Verification step',
  verification_start: 'Verification step',
  verification_step: 'Verification step',
  verification_cancelled: 'Verification step',
  verification_done: 'Verification step',
  call_started: 'Call event',
  call_answered: 'Call event',
  call_declined: 'Call event',
  call_ended: 'Call event',
  call_signalling: 'Call connection details',
  poll_started: 'Poll',
  poll_vote: 'Poll vote',
  poll_ended: 'End of a poll',
  sticker: 'Sticker',
  location: 'Shared location',
  message: 'Message'
};

/** The pieces of the line for a deleted other event from `sender`. */
export function deletedParts(sender: string, event: OtherEvent, eventType: string): StatePart[] {
  const what = Object.hasOwn(DELETED_TEXT, event) ? DELETED_TEXT[event] : undefined;
  if (what !== undefined) return [{ person: sender }, { text: `: ${what} deleted.` }];
  return [{ person: sender }, { text: ': Event of type ' }, { name: eventType }, { text: ' deleted.' }];
}

/**
 * The switch for each kind. A message has none: a message of a type this
 * build has no text for is still a message, and messages always show.
 */
type OtherSwitch = Extract<
  keyof TimelineEvents,
  | 'verification'
  | 'calls'
  | 'callSignalling'
  | 'polls'
  | 'stickers'
  | 'locations'
  | 'spaces'
  | 'widgets'
  | 'invitations'
  | 'otherEvents'
>;

const OTHER_SWITCH: Record<OtherEvent, OtherSwitch | null> = {
  verification_request: 'verification',
  verification_ready: 'verification',
  verification_start: 'verification',
  verification_step: 'verification',
  verification_cancelled: 'verification',
  verification_done: 'verification',
  call_started: 'calls',
  call_answered: 'calls',
  call_declined: 'calls',
  call_ended: 'calls',
  call_joined: 'calls',
  call_left: 'calls',
  call_signalling: 'callSignalling',
  call_updated: 'callSignalling',
  poll_started: 'polls',
  poll_vote: 'polls',
  poll_ended: 'polls',
  sticker: 'stickers',
  location: 'locations',
  location_stopped: 'locations',
  space_child: 'spaces',
  space_parent: 'spaces',
  widget: 'widgets',
  third_party_invite: 'invitations',
  message: null,
  event: 'otherEvents',
  state: 'otherEvents'
};

/**
 * The settings switch that shows or hides an other event of this kind, or
 * `null` for a message, which no switch hides.
 */
export function otherSwitch(event: OtherEvent): keyof TimelineEvents | null {
  return Object.hasOwn(OTHER_SWITCH, event) ? OTHER_SWITCH[event] : 'otherEvents';
}

/**
 * Whether the timeline shows `item` with these settings. Messages, images,
 * files, gaps and encryption notices always show: hiding a message that
 * could not be decrypted would hide that it exists.
 */
export function itemShown(item: ShownItem, events: TimelineEvents): boolean {
  switch (item.kind) {
    case 'state':
      return changeShown(item.change, events);
    case 'other': {
      // A deleted one shows only while its kind and deleted lines show.
      const key = otherSwitch(item.event);
      const kindShown = key === null || events[key] !== false;
      return kindShown && (!item.deleted || events.deletedMessages !== false);
    }
    case 'notice':
      return item.notice !== 'deleted' || events.deletedMessages !== false;
    default:
      return true;
  }
}

/** The switches for other events, in the order the settings list them. */
export const OTHER_SWITCHES: readonly { key: keyof TimelineEvents; label: string }[] = [
  { key: 'verification', label: 'Verification requests and steps' },
  { key: 'calls', label: 'Calls, and people joining or leaving them' },
  { key: 'callSignalling', label: 'Call connection details and membership updates' },
  { key: 'polls', label: 'Polls and votes' },
  { key: 'stickers', label: 'Stickers' },
  { key: 'locations', label: 'Shared locations' },
  { key: 'spaces', label: 'Space changes' },
  { key: 'widgets', label: 'Widget changes' },
  { key: 'deletedMessages', label: 'Deleted messages and events' },
  { key: 'otherEvents', label: 'Other kinds of events, shown by their type' }
];
