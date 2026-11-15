// The line shown for a change to a room's members or state, and which
// setting shows or hides it. Pure functions, no host calls.
import type { StateChange, TimelineEvents } from '../../crates/contracts/generated/contracts';
import { personText } from './people.ts';
import { historyText, joinRuleText } from './roomSettings.ts';

/**
 * One piece of a state line: fixed text, a person (a user ID, shown with
 * their display name when there is one), or a name, user ID, room name or
 * topic from the event shown as it is. The page shows each `person` and
 * `name` piece in its own <bdi>, so bidirectional text in one cannot reorder
 * the rest of the line.
 */
export type StatePart = { text: string } | { person: string } | { name: string };

const text = (value: string): StatePart => ({ text: value });
const name = (value: string): StatePart => ({ name: value });
const person = (userId: string): StatePart => ({ person: userId });

/** The pieces of the line for `change`, made by `sender`. */
export function stateParts(sender: string, change: StateChange): StatePart[] {
  const by = person(sender);
  switch (change.type) {
    case 'member': {
      const target = person(change.target);
      switch (change.membership) {
        case 'joined':
          return [target, text(' joined')];
        case 'left':
          return [target, text(' left')];
        case 'invited':
          return [by, text(' invited '), target];
        case 'invite_rejected':
          return [target, text(' declined the invitation')];
        case 'invite_withdrawn':
          return [by, text(' withdrew the invitation for '), target];
        case 'kicked':
          return [by, text(' removed '), target];
        case 'banned':
          return [by, text(' banned '), target];
        case 'unbanned':
          return [by, text(' unbanned '), target];
        case 'knocked':
          return [target, text(' asked to join')];
        case 'knock_withdrawn':
          return [target, text(' withdrew their request to join')];
        case 'knock_denied':
          return [by, text(' refused the request to join from '), target];
      }
      break;
    }
    // The ID alone: the person's current name could be the new one.
    case 'display_name':
      return change.name === null
        ? [name(change.target), text(' removed their display name')]
        : [name(change.target), text(' is now shown as '), name(change.name)];
    case 'profile_picture':
      return [person(change.target), text(' changed their profile picture')];
    case 'name':
      return change.name === null
        ? [by, text(' removed the room name')]
        : [by, text(' renamed the room to '), name(change.name)];
    case 'topic':
      return change.topic === null
        ? [by, text(' removed the topic')]
        : [by, text(' changed the topic to '), name(change.topic)];
    case 'avatar':
      return [by, text(' changed the room picture')];
    case 'encryption':
      return [by, text(' turned on end-to-end encryption')];
    case 'join_rule':
      return [by, text(' changed who can join: ' + joinRuleText[change.rule])];
    case 'history_visibility':
      return [by, text(' changed who can read the history: ' + historyText[change.visibility])];
    case 'power_levels':
      return [by, text(' changed what members may do')];
    case 'addresses':
      return [by, text(" changed the room's addresses")];
    case 'created':
      return [by, text(' created the room')];
    case 'server_acl':
      return [by, text(' changed which servers may take part')];
    case 'upgraded':
      return [by, text(' replaced this room with a new version')];
    case 'pinned_messages':
      return [by, text(' changed the pinned messages')];
    case 'guest_access':
      return [by, text(change.allowed ? ' let guests join' : ' stopped guests from joining')];
  }
  // A change this build does not know (the core never sends one).
  return [by, text(' changed the room')];
}

/**
 * The whole line as plain text, for an accessible label or a test. A person
 * reads as "Name (@id)" when `names` has their display name.
 */
export function stateLine(
  sender: string,
  change: StateChange,
  names: ReadonlyMap<string, string> = new Map()
): string {
  return stateParts(sender, change)
    .map((part) =>
      'person' in part ? personText(part.person, names) : 'name' in part ? part.name : part.text
    )
    .join('');
}

/** The settings switch that shows or hides `change`, or `null` for one no switch hides. */
export function eventSwitch(change: StateChange): keyof TimelineEvents | null {
  switch (change.type) {
    case 'member':
      switch (change.membership) {
        case 'joined':
          return 'joins';
        case 'left':
          return 'leaves';
        case 'invited':
        case 'invite_rejected':
        case 'invite_withdrawn':
          return 'invitations';
        case 'kicked':
        case 'banned':
        case 'unbanned':
          return 'removals';
        case 'knocked':
        case 'knock_withdrawn':
        case 'knock_denied':
          return 'joinRequests';
      }
      return null;
    case 'display_name':
      return 'displayNames';
    case 'profile_picture':
      return 'profilePictures';
    case 'name':
      return 'roomName';
    case 'topic':
      return 'topic';
    case 'avatar':
      return 'roomPicture';
    case 'encryption':
      return 'encryption';
    case 'join_rule':
      return 'joinRule';
    case 'history_visibility':
      return 'historyVisibility';
    case 'power_levels':
      return 'permissions';
    case 'addresses':
      return 'addresses';
    case 'pinned_messages':
      return 'pinnedMessages';
    case 'guest_access':
      return 'guestAccess';
    case 'server_acl':
      return 'serverAccess';
    case 'created':
      return 'creation';
    case 'upgraded':
      return 'upgrades';
  }
  return null;
}

/** Whether the timeline shows `change` with these settings. A change no switch names always shows. */
export function changeShown(change: StateChange, events: TimelineEvents): boolean {
  const key = eventSwitch(change);
  return key === null || events[key] !== false;
}

/** The switches in the order the settings list them, with their labels. */
export const EVENT_SWITCHES: readonly { key: keyof TimelineEvents; label: string }[] = [
  { key: 'joins', label: 'Someone joins' },
  { key: 'leaves', label: 'Someone leaves' },
  { key: 'invitations', label: 'Invitations, declined and withdrawn' },
  { key: 'removals', label: 'Someone is removed, banned or unbanned' },
  { key: 'joinRequests', label: 'Requests to join' },
  { key: 'displayNames', label: 'Display name changes' },
  { key: 'profilePictures', label: 'Profile picture changes' },
  { key: 'roomName', label: 'Room name changes' },
  { key: 'topic', label: 'Topic changes' },
  { key: 'roomPicture', label: 'Room picture changes' },
  { key: 'encryption', label: 'Encryption turned on' },
  { key: 'joinRule', label: 'Who can join' },
  { key: 'historyVisibility', label: 'Who can read the history' },
  { key: 'permissions', label: 'What members may do' },
  { key: 'addresses', label: 'Room addresses' },
  { key: 'pinnedMessages', label: 'Pinned messages' },
  { key: 'guestAccess', label: 'Guest access' },
  { key: 'serverAccess', label: 'Which servers may take part' },
  { key: 'creation', label: 'Room created' },
  { key: 'upgrades', label: 'Room replaced by a new version' }
];
