// Text and choices for the room settings. Pure functions; every name, topic
// and address they get is rendered as escaped text only. The permissions in
// the settings are hints: the homeserver decides.
import type {
  HistoryVisibility,
  JoinRule,
  LevelKey,
  Membership,
  RoomChange,
  RoomMembers,
  RoomNotifications,
  RoomSettings
} from '../../crates/contracts/generated/contracts';

/** The levels with a plain-word name; any other level is shown as a number. */
export const ROLES: readonly { level: number; label: string }[] = [
  { level: 100, label: 'Admin' },
  { level: 50, label: 'Moderator' },
  { level: 0, label: 'Member' }
];

/** JavaScript's largest exact integer, and the Matrix limit for a level. */
const MAX_LEVEL = Number.MAX_SAFE_INTEGER;

/** A role name for a level; `null` is a room creator with every permission. */
export function roleOf(level: number | null): string {
  if (level === null) return 'Creator';
  return ROLES.find((role) => role.level === level)?.label ?? `Level ${level}`;
}

/**
 * The levels to offer for a setting or a member now at `current`, when this
 * account is at `own` (`null`: a creator, with no upper bound). Nobody can
 * give a level above their own, so roles above it are left out; the current
 * level is always offered, so the choice shows it.
 */
export function levelChoices(
  current: number,
  own: number | null
): { level: number; label: string }[] {
  const choices = ROLES.filter((role) => own === null || role.level <= own);
  if (!choices.some((choice) => choice.level === current)) {
    choices.push({ level: current, label: roleOf(current) });
  }
  return [...choices].sort((a, b) => b.level - a.level);
}

/**
 * A level typed by the user: a whole number within the Matrix limit, or
 * `null` when the text is not one.
 */
export function parseLevel(text: string): number | null {
  const trimmed = text.trim();
  if (!/^-?\d+$/.test(trimmed)) return null;
  const level = Number(trimmed);
  return Number.isSafeInteger(level) && Math.abs(level) <= MAX_LEVEL ? level : null;
}

export const levelLabels: Record<LevelKey, string> = {
  users_default: 'Default level of new members',
  events_default: 'Send messages',
  state_default: 'Change other settings',
  invite: 'Invite people',
  kick: 'Remove people',
  ban: 'Ban people',
  redact: "Remove other people's messages",
  notify_room: 'Notify everyone (@room)',
  name: 'Change the name',
  topic: 'Change the topic',
  avatar: 'Change the picture',
  canonical_alias: 'Change the addresses',
  join_rules: 'Change who can join',
  history_visibility: 'Change who can read history',
  guest_access: 'Change guest access',
  encryption: 'Turn on encryption',
  power_levels: 'Change permissions and roles',
  server_acl: 'Change which servers may take part',
  tombstone: 'Upgrade the room',
  pinned_events: 'Pin messages'
};

/** The power levels in the order and groups the settings show them. */
export const levelGroups: readonly { title: string; keys: readonly LevelKey[] }[] = [
  { title: 'Members', keys: ['users_default'] },
  { title: 'Messages', keys: ['events_default', 'notify_room', 'redact', 'pinned_events'] },
  { title: 'Moderation', keys: ['invite', 'kick', 'ban'] },
  {
    title: 'Settings',
    keys: [
      'name',
      'topic',
      'avatar',
      'canonical_alias',
      'join_rules',
      'history_visibility',
      'guest_access',
      'encryption',
      'power_levels',
      'server_acl',
      'tombstone',
      'state_default'
    ]
  }
];

/** The level a setting needs, from the settings' list; 0 if it is missing. */
export function levelOf(settings: RoomSettings, key: LevelKey): number {
  return settings.levels.find((entry) => entry.key === key)?.level ?? 0;
}

export const joinRuleText: Record<JoinRule, string> = {
  invite: 'Only people who are invited',
  knock: 'Anyone can ask to join; a member lets them in',
  public: 'Anyone who finds the room',
  restricted: 'Members of certain other rooms (set in another client)',
  knock_restricted: 'Members of certain other rooms, and anyone can ask (set in another client)',
  private: 'Only people who are invited',
  other: 'A rule Persephone does not know'
};

/** The join rules Persephone can set, in the order offered. */
export const JOIN_RULES: readonly JoinRule[] = ['invite', 'knock', 'public'];

export const historyText: Record<HistoryVisibility, string> = {
  joined: 'Members, from when they joined',
  invited: 'Members, from when they were invited',
  shared: 'Members, including messages sent before they joined',
  world_readable: 'Anyone, members or not',
  other: 'A setting Persephone does not know'
};

/** The history settings Persephone can set, from the most private. */
export const HISTORY: readonly HistoryVisibility[] = ['joined', 'invited', 'shared', 'world_readable'];

export const notificationsText: Record<RoomNotifications, string> = {
  default: 'Use your account default',
  all: 'All messages',
  mentions: 'Mentions and keywords only',
  mute: 'Off'
};

export const NOTIFICATIONS: readonly RoomNotifications[] = ['default', 'all', 'mentions', 'mute'];

export const membershipText: Record<Membership, string | null> = {
  join: null,
  invite: 'Invited',
  knock: 'Asking to join',
  ban: 'Banned'
};

/** A first check that text is a room address (`#name:server`); the core decides. */
export function looksLikeAlias(text: string): boolean {
  return text.length <= 255 && /^#[^\s:]+:\S+$/.test(text);
}

/** One address of a room, and where it is known from. */
export type Address = {
  alias: string;
  main: boolean;
  published: boolean;
  /** On this account's homeserver's directory. */
  local: boolean;
};

/**
 * Every address the settings name, once each: the main address first, then
 * the published ones, then those only on this homeserver.
 */
export function addresses(settings: RoomSettings): Address[] {
  const local = settings.localAliases ?? [];
  const all = [
    ...(settings.canonicalAlias ? [settings.canonicalAlias] : []),
    ...settings.altAliases,
    ...local
  ];
  return [...new Set(all)].map((alias) => ({
    alias,
    main: alias === settings.canonicalAlias,
    published: alias === settings.canonicalAlias || settings.altAliases.includes(alias),
    local: local.includes(alias)
  }));
}

/**
 * The settings as they are after the homeserver accepted `change`. The
 * device's copy of the room's state changes only with the next sync, so the
 * page shows the change at once and reads the settings again later.
 * Changes to members and leaving change nothing here.
 */
export function applyLocally(settings: RoomSettings, change: RoomChange): RoomSettings {
  switch (change.kind) {
    case 'name':
      return { ...settings, name: change.name.trim() || null };
    case 'topic':
      return { ...settings, topic: change.topic.trim() || null };
    case 'join_rule':
      return { ...settings, joinRule: change.rule };
    case 'history_visibility':
      return { ...settings, historyVisibility: change.visibility };
    case 'guest_access':
      return { ...settings, guestAccess: change.allowed };
    case 'encryption':
      return { ...settings, encryption: 'encrypted', may: { ...settings.may, encryption: false } };
    case 'published':
      return { ...settings, published: change.published };
    case 'add_alias':
      return {
        ...settings,
        localAliases: [...new Set([...(settings.localAliases ?? []), change.alias])]
      };
    case 'remove_alias':
      return {
        ...settings,
        canonicalAlias: settings.canonicalAlias === change.alias ? null : settings.canonicalAlias,
        altAliases: settings.altAliases.filter((alias) => alias !== change.alias),
        localAliases: settings.localAliases?.filter((alias) => alias !== change.alias) ?? null
      };
    case 'canonical_alias':
      return { ...settings, canonicalAlias: change.alias, altAliases: change.altAliases };
    case 'level':
      return {
        ...settings,
        levels: settings.levels.map((entry) =>
          entry.key === change.key ? { ...entry, level: change.level } : entry
        )
      };
    case 'notifications':
      return { ...settings, notifications: change.mode };
    default:
      return settings;
  }
}

/**
 * The member list as it is after the homeserver accepted `change`: a level
 * set, a removed or unbanned person gone, a banned one marked. The next read
 * corrects anything else, such as what this account may now do to them.
 */
export function applyToMembers(members: RoomMembers, change: RoomChange): RoomMembers {
  switch (change.kind) {
    case 'member_level':
      return {
        ...members,
        members: members.members.map((member) =>
          member.userId === change.userId ? { ...member, level: change.level } : member
        )
      };
    case 'kick':
    case 'unban':
      return {
        ...members,
        members: members.members.filter((member) => member.userId !== change.userId)
      };
    case 'ban':
      return {
        ...members,
        members: members.members.map((member) =>
          member.userId === change.userId
            ? { ...member, membership: 'ban', canKick: false, canBan: false, canChangeLevel: false }
            : member
        )
      };
    default:
      return members;
  }
}

const failures: Record<string, string> = {
  rejected: 'The homeserver refused. You may not have permission to change this.',
  core_unavailable: 'The homeserver could not be reached.',
  limit_exceeded: 'Another change is being made. Try again in a moment.',
  locked: 'The session is locked.',
  not_found: 'You are no longer in this room, or that is gone.',
  invalid_argument: 'That value is not valid here.'
};

/** Fixed text for an error code from the host; nothing else is shown. */
export function failureText(reason: unknown): string {
  return failures[String(reason)] ?? 'That did not work.';
}
