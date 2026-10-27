import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as rooms from '../src/roomSettings.ts';

const settings = {
  roomId: '!r:example.invalid',
  name: 'Plans',
  topic: null,
  canonicalAlias: '#plans:example.invalid',
  altAliases: ['#alt:example.invalid'],
  localAliases: ['#plans:example.invalid', '#mine:example.invalid'],
  joinRule: 'invite',
  historyVisibility: 'shared',
  guestAccess: false,
  encryption: 'not_encrypted',
  published: false,
  roomVersion: '11',
  direct: false,
  notifications: 'default',
  levels: [
    { key: 'ban', level: 50 },
    { key: 'kick', level: 50 }
  ],
  ownLevel: 100,
  may: {
    name: true,
    topic: true,
    canonicalAlias: true,
    joinRules: true,
    historyVisibility: true,
    guestAccess: true,
    encryption: true,
    powerLevels: true,
    invite: true,
    kick: true,
    ban: true
  }
};

test('GIVEN power levels WHEN named THEN the usual ones have plain words and others a number', () => {
  const { roleOf } = rooms;
  assert.equal(roleOf(100), 'Admin');
  assert.equal(roleOf(50), 'Moderator');
  assert.equal(roleOf(0), 'Member');
  assert.equal(roleOf(75), 'Level 75');
  assert.equal(roleOf(-1), 'Level -1');
  assert.equal(roleOf(null), 'Creator');
});

test('GIVEN this account\'s level WHEN levels are offered THEN none above it, and the current one always', () => {
  const { levelChoices } = rooms;
  const levels = (current, own) => levelChoices(current, own).map((choice) => choice.level);
  assert.deepEqual(levels(0, 100), [100, 50, 0]);
  assert.deepEqual(levels(0, 50), [50, 0]);
  assert.deepEqual(levels(0, null), [100, 50, 0]);
  // A custom level is offered, in order, so the choice can show it.
  assert.deepEqual(levels(75, 100), [100, 75, 50, 0]);
  // A level above this account's own is still shown, though not offered.
  assert.deepEqual(levels(100, 50), [100, 50, 0]);
});

test('GIVEN typed levels WHEN read THEN only whole numbers within the Matrix limit pass', () => {
  const { parseLevel } = rooms;
  assert.equal(parseLevel(' 42 '), 42);
  assert.equal(parseLevel('-5'), -5);
  assert.equal(parseLevel('9007199254740991'), 9007199254740991);
  for (const invalid of ['', '1.5', '1e3', '0x10', 'ten', '9007199254740992', '- 1', '+1']) {
    assert.equal(parseLevel(invalid), null, invalid);
  }
});

test('GIVEN every level key WHEN shown THEN each has a label and appears in exactly one group', () => {
  const { levelGroups, levelLabels } = rooms;
  const grouped = levelGroups.flatMap((group) => group.keys);
  assert.equal(new Set(grouped).size, grouped.length);
  assert.deepEqual([...grouped].sort(), Object.keys(levelLabels).sort());
  assert.equal(grouped.length, 20);
});

test('GIVEN the settings WHEN addresses are listed THEN each appears once, the main one first', () => {
  const listed = rooms.addresses(settings);
  assert.deepEqual(listed, [
    { alias: '#plans:example.invalid', main: true, published: true, local: true },
    { alias: '#alt:example.invalid', main: false, published: true, local: false },
    { alias: '#mine:example.invalid', main: false, published: false, local: true }
  ]);
  assert.deepEqual(rooms.addresses({ ...settings, canonicalAlias: null, altAliases: [], localAliases: null }), []);
});

test('GIVEN a room address WHEN checked THEN only #name:server passes', () => {
  const { looksLikeAlias } = rooms;
  assert.ok(looksLikeAlias('#plans:example.org'));
  for (const invalid of ['plans:example.org', '#plans', '!r:example.org', '# a:b', '#' + 'a'.repeat(255) + ':s']) {
    assert.ok(!looksLikeAlias(invalid), invalid);
  }
});

test('GIVEN an accepted change WHEN applied locally THEN the settings show it at once', () => {
  const { applyLocally } = rooms;
  assert.equal(applyLocally(settings, { kind: 'name', name: '  ' }).name, null);
  assert.equal(applyLocally(settings, { kind: 'topic', topic: ' Hi ' }).topic, 'Hi');
  assert.equal(applyLocally(settings, { kind: 'join_rule', rule: 'knock' }).joinRule, 'knock');
  const encrypted = applyLocally(settings, { kind: 'encryption' });
  assert.equal(encrypted.encryption, 'encrypted');
  assert.equal(encrypted.may.encryption, false);
  assert.equal(settings.encryption, 'not_encrypted', 'the original is not changed');
  const removed = applyLocally(settings, { kind: 'remove_alias', alias: '#plans:example.invalid' });
  assert.equal(removed.canonicalAlias, null);
  assert.deepEqual(removed.localAliases, ['#mine:example.invalid']);
  const added = applyLocally({ ...settings, localAliases: null }, { kind: 'add_alias', alias: '#n:s' });
  assert.deepEqual(added.localAliases, ['#n:s']);
  const levelled = applyLocally(settings, { kind: 'level', key: 'kick', level: 0 });
  assert.deepEqual(levelled.levels, [
    { key: 'ban', level: 50 },
    { key: 'kick', level: 0 }
  ]);
  assert.equal(applyLocally(settings, { kind: 'leave' }), settings);
});

test('GIVEN an accepted member change WHEN applied to the list THEN levels, removals and bans show', () => {
  const member = (userId, level) => ({
    userId,
    displayName: null,
    membership: 'join',
    level,
    own: false,
    canKick: true,
    canBan: true,
    canUnban: false,
    canChangeLevel: true
  });
  const members = { roomId: '!r:s', truncated: false, members: [member('@a:s', 0), member('@b:s', 50)] };
  const { applyToMembers } = rooms;
  assert.equal(applyToMembers(members, { kind: 'member_level', userId: '@a:s', level: 50 }).members[0].level, 50);
  assert.deepEqual(
    applyToMembers(members, { kind: 'kick', userId: '@a:s' }).members.map((m) => m.userId),
    ['@b:s']
  );
  const banned = applyToMembers(members, { kind: 'ban', userId: '@b:s' }).members[1];
  assert.equal(banned.membership, 'ban');
  assert.equal(banned.canBan, false);
  assert.equal(banned.canChangeLevel, false);
});

test('GIVEN host error codes WHEN shown THEN each is fixed text, and anything else is generic', () => {
  const { failureText } = rooms;
  assert.match(failureText('rejected'), /refused/);
  assert.match(failureText('limit_exceeded'), /Another change/);
  assert.equal(failureText('<img src=x>'), 'That did not work.');
  assert.equal(failureText(new Error('C:\\secret\\path')), 'That did not work.');
});
