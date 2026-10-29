// The line shown for a change to a room's members or state. Pure functions,
// no host calls.
import type { StateChange } from '../../crates/contracts/generated/contracts';
import { historyText, joinRuleText } from './roomSettings.ts';

/**
 * One piece of a state line: fixed text, or a name, user ID, room name or
 * topic from the event. The page shows each `name` piece in its own <bdi>,
 * so bidirectional text in one cannot reorder the rest of the line.
 */
export type StatePart = { text: string } | { name: string };

const text = (value: string): StatePart => ({ text: value });
const name = (value: string): StatePart => ({ name: value });

/** The pieces of the line for `change`, made by `sender`. */
export function stateParts(sender: string, change: StateChange): StatePart[] {
  const by = name(sender);
  switch (change.type) {
    case 'member': {
      const target = name(change.target);
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
    case 'display_name':
      return change.name === null
        ? [name(change.target), text(' removed their display name')]
        : [name(change.target), text(' is now shown as '), name(change.name)];
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
  }
  // A change this build does not know (the core never sends one).
  return [by, text(' changed the room')];
}

/** The whole line as plain text, for an accessible label or a test. */
export function stateLine(sender: string, change: StateChange): string {
  return stateParts(sender, change)
    .map((part) => ('name' in part ? part.name : part.text))
    .join('');
}
