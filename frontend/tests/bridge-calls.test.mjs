import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { before, test } from 'node:test';
import { mockIPC } from '@tauri-apps/api/mocks';

// The bridge's calls must match the host: a command the host registers, with
// the argument names its Rust function declares (Tauri renames snake_case
// arguments to camelCase). A mismatch fails only at run time in the app, so
// these tests read the host's sources instead of repeating the bridge.

let bridge;
let registered;
let hostArguments;

const camel = (name) => name.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase());

before(async () => {
  const build = await readFile(new URL('../../src-tauri/build.rs', import.meta.url), 'utf8');
  const list = build.slice(build.indexOf('.commands(&['), build.indexOf(']))'));
  registered = new Set([...list.matchAll(/"([a-z_]+)"/g)].map((match) => match[1]));

  // `async fn name(core: State<'_, CoreLink>, room_id: String, ...)`: the
  // arguments the page sends, without Tauri's own State, Window and Channel.
  const host = await readFile(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  hostArguments = new Map();
  for (const [, name, parameters] of host.matchAll(/async fn ([a-z_]+)\(([^)]*)\)/g)) {
    const names = [...parameters.matchAll(/([a-z_]+):\s*([A-Za-z]+)/g)]
      .filter(([, , type]) => !['State', 'Window', 'WebviewWindow', 'AppHandle'].includes(type))
      .map(([, parameter]) => camel(parameter));
    hostArguments.set(name, names.sort());
  }

  // The bridge's invoke goes through window.__TAURI_INTERNALS__.
  globalThis.window = globalThis;
  bridge = await import('../src/bridge/index.ts');
});

async function sent(call, reply = null) {
  const calls = [];
  mockIPC((command, args) => {
    calls.push({ command, args });
    return reply;
  });
  await call();
  assert.equal(calls.length, 1);
  return calls[0];
}

test('GIVEN the room, account, settings and profile calls WHEN made THEN each matches a host command and its arguments', async () => {
  const cases = [
    [() => bridge.createRoom('Plans', ['@b:s']), 'create_room', { name: 'Plans', invite: ['@b:s'] }],
    [() => bridge.inviteUser('!r:s', '@b:s'), 'invite_user', { roomId: '!r:s', userId: '@b:s' }],
    [() => bridge.displayName(), 'display_name', {}],
    [() => bridge.sendMessage('!r:s', 'hi'), 'send_message', { roomId: '!r:s', body: 'hi', replyTo: null }],
    [() => bridge.deleteMessage('!r:s', '$mine'), 'delete_message', { roomId: '!r:s', eventId: '$mine' }],
    [() => bridge.react('!r:s', '$e', '👍'), 'react', { roomId: '!r:s', eventId: '$e', key: '👍' }],
    [() => bridge.markRead('!r:s', '$e'), 'mark_read', { roomId: '!r:s', eventId: '$e' }],
    [() => bridge.pickFile('!r:s'), 'pick_file', { roomId: '!r:s' }],
    [() => bridge.saveFile('!r:s', '$f'), 'save_file', { roomId: '!r:s', eventId: '$f' }],
    [() => bridge.roomActivity('!r:s', '$e'), 'room_activity', { roomId: '!r:s', eventId: '$e' }],
    [() => bridge.roomActivity('!r:s', null), 'room_activity', { roomId: '!r:s', eventId: null }],
    [() => bridge.setTyping('!r:s', true), 'set_typing', { roomId: '!r:s', typing: true }],
    [() => bridge.openLink('https://example.org/'), 'open_link', { url: 'https://example.org/' }],
    [() => bridge.roomServers('!r:s'), 'room_servers', { roomId: '!r:s' }],
    [() => bridge.roomReaders('!r:s'), 'room_readers', { roomId: '!r:s' }],
    [
      () => bridge.editMessage('!r:s', '$mine', 'fixed'),
      'edit_message',
      { roomId: '!r:s', eventId: '$mine', body: 'fixed' }
    ],
    [
      () => bridge.sendMessage('!r:s', 'yes', '$asked'),
      'send_message',
      { roomId: '!r:s', body: 'yes', replyTo: '$asked' }
    ],
    [() => bridge.setDisplayName('Al'), 'set_display_name', { name: 'Al' }],
    [() => bridge.deviceList(), 'device_list', {}],
    [() => bridge.accountSettings(), 'account_settings', {}],
    [() => bridge.roomDraft('!r:s'), 'room_draft', { roomId: '!r:s' }],
    [() => bridge.roomDrafts(), 'room_drafts', {}],
    [() => bridge.saveRoomDraft('!r:s', { body: 'draft', replyTo: '$e' }), 'save_room_draft', { roomId: '!r:s', draft: { body: 'draft', replyTo: '$e' } }],
    [() => bridge.roomPreferences(), 'room_preferences', {}],
    [() => bridge.setRoomPreference({ roomId: '!r:s', favourite: true, section: 'Work' }), 'set_room_preference', { preference: { roomId: '!r:s', favourite: true, section: 'Work' } }],
    [() => bridge.searchMessages('hello', null), 'search_messages', { term: 'hello', roomId: null, filters: { sender: null, fromMs: null, beforeMs: null } }],
    [() => bridge.searchMessages('hello', '!r:s'), 'search_messages', { term: 'hello', roomId: '!r:s', filters: { sender: null, fromMs: null, beforeMs: null } }],
    [() => bridge.searchMessages('hello', '!r:s', { sender: '@a:s', fromMs: 1, beforeMs: 2 }), 'search_messages', { term: 'hello', roomId: '!r:s', filters: { sender: '@a:s', fromMs: 1, beforeMs: 2 } }],
    [() => bridge.jumpToDate('!r:s', 123), 'jump_to_date', { roomId: '!r:s', timestampMs: 123 }],
    [() => bridge.eventContext('!r:s', '$e'), 'event_context', { roomId: '!r:s', eventId: '$e' }],
    [() => bridge.pinnedMessages('!r:s'), 'pinned_messages', { roomId: '!r:s' }],
    [() => bridge.setPinned('!r:s', '$e', true), 'set_pinned', { roomId: '!r:s', eventId: '$e', pinned: true }],
    [() => bridge.setPinned('!r:s', '$e', false), 'set_pinned', { roomId: '!r:s', eventId: '$e', pinned: false }],
    [() => bridge.attachments('!r:s', 'cursor', 'images', { sender: '@a:s', fromMs: 1, beforeMs: 2 }), 'attachments', { roomId: '!r:s', cursor: 'cursor', kind: 'images', filters: { sender: '@a:s', fromMs: 1, beforeMs: 2 } }],
    [() => bridge.attachments('!r:s', null, 'all', { sender: null, fromMs: null, beforeMs: null }), 'attachments', { roomId: '!r:s', cursor: null, kind: 'all', filters: { sender: null, fromMs: null, beforeMs: null } }],
    [() => bridge.connectionHealth(), 'connection_health', {}],
    [() => bridge.ignoredUsers(), 'ignored_users', {}],
    [() => bridge.setIgnoredUser('@a:s', true), 'set_ignored_user', { userId: '@a:s', ignored: true }],
    [() => bridge.reportAbuse('!r:s', '$e', 'spam'), 'report_abuse', { roomId: '!r:s', eventId: '$e', reason: 'spam' }],
    [
      () =>
        bridge.setAccountSettings({
          enterSends: false,
          chatTextPercent: 120,
          interfaceTextPercent: 90,
          desktopNotifications: true,
          sendTyping: true,
          sendReadReceipts: false,
          idleLockMinutes: 15,
          notificationNames: false
        }),
      'set_account_settings',
      {
        settings: {
          enterSends: false,
          chatTextPercent: 120,
          interfaceTextPercent: 90,
          desktopNotifications: true,
          sendTyping: true,
          sendReadReceipts: false,
          idleLockMinutes: 15,
          notificationNames: false
        }
      }
    ],
    [() => bridge.changePassword(true), 'change_password', { signOutOthers: true }],
    [() => bridge.newSessions(), 'new_sessions', {}],
    [() => bridge.signInWithOidc('https://e.org'), 'sign_in_with_oidc', { homeserver: 'https://e.org' }],
    [() => bridge.proxySetting(), 'proxy_setting', {}],
    [() => bridge.setProxySetting('127.0.0.1:9050'), 'set_proxy_setting', { proxy: '127.0.0.1:9050' }],
    [() => bridge.setProxySetting(null), 'set_proxy_setting', { proxy: null }],
    [() => bridge.discoverHomeserver('example.org'), 'discover_homeserver', { serverName: 'example.org' }],
    [() => bridge.publicRooms(null, null, null), 'public_rooms', { server: null, filter: null, since: null }],
    [() => bridge.publicRooms('example.org', 'rust', 'p30'), 'public_rooms', { server: 'example.org', filter: 'rust', since: 'p30' }],
    [() => bridge.joinPublicRoom('!r:s'), 'join_public_room', { roomId: '!r:s' }],
    [() => bridge.acknowledgeSessions(['D']), 'acknowledge_sessions', { deviceIds: ['D'] }],
    [() => bridge.removeDevice('D'), 'remove_device', { deviceId: 'D' }],
    [() => bridge.threePids(), 'three_pids', {}],
    [() => bridge.requestEmailToken('a@example.org'), 'request_email_token', { email: 'a@example.org' }],
    [
      () => bridge.requestPhoneToken('GB', '07700900123'),
      'request_phone_token',
      { country: 'GB', phoneNumber: '07700900123' }
    ],
    [() => bridge.addThreePid(), 'add_three_pid', {}],
    [
      () => bridge.removeThreePid('email', 'a@example.org'),
      'remove_three_pid',
      { medium: 'email', address: 'a@example.org' }
    ],
    [() => bridge.verifyDevice('OTHER'), 'verify_device', { deviceId: 'OTHER' }],
    [() => bridge.verifyMember('!r:s', '@b:s'), 'verify_member', { roomId: '!r:s', userId: '@b:s' }],
    [() => bridge.changeRecoveryKey(), 'change_recovery_key', {}],
    [() => bridge.notificationLevel(), 'notification_level', {}],
    [() => bridge.setNotificationLevel('mentions'), 'set_notification_level', { level: 'mentions' }],
    [() => bridge.profileList(), 'profile_list', {}],
    [() => bridge.addProfile(), 'add_profile', {}],
    [() => bridge.switchProfile(2), 'switch_profile', { slot: 2 }],
    [() => bridge.register('https://h', 'al'), 'register', { homeserver: 'https://h', username: 'al' }],
    [() => bridge.roomSettings('!r:s'), 'room_settings', { roomId: '!r:s' }],
    [() => bridge.roomMembers('!r:s', 'bo'), 'room_members', { roomId: '!r:s', filter: 'bo' }],
    [
      () => bridge.changeRoom('!r:s', { kind: 'join_rule', rule: 'knock' }),
      'change_room',
      { roomId: '!r:s', change: { kind: 'join_rule', rule: 'knock' } }
    ]
  ];
  for (const [call, command, args] of cases) {
    const made = await sent(call);
    assert.equal(made.command, command);
    assert.deepEqual(made.args, args, command);
    assert.ok(registered.has(command), `${command} is registered in build.rs`);
    assert.deepEqual(Object.keys(args).sort(), hostArguments.get(command), `${command} arguments`);
  }
});

test('GIVEN account settings WHEN saved THEN only the known settings cross, never extra fields', async () => {
  const made = await sent(() =>
    bridge.setAccountSettings({
      enterSends: true,
      chatTextPercent: 100,
      interfaceTextPercent: 100,
      desktopNotifications: false,
      sendTyping: false,
      sendReadReceipts: false,
      idleLockMinutes: 0,
      notificationNames: true,
      theme: 'dark'
    })
  );
  assert.deepEqual(made.args, {
    settings: {
      enterSends: true,
      chatTextPercent: 100,
      interfaceTextPercent: 100,
      desktopNotifications: false,
      sendTyping: false,
      sendReadReceipts: false,
      idleLockMinutes: 0,
      notificationNames: true
    }
  });
});

test('GIVEN the account-change calls WHEN made THEN no password, code, key or path argument exists', async () => {
  const host = await readFile(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  for (const command of [
    'change_password',
    'add_three_pid',
    'change_recovery_key',
    'request_phone_token',
    'verify_device'
  ]) {
    const names = hostArguments.get(command) ?? [];
    assert.ok(host.includes(`async fn ${command}(`), command);
    for (const name of names) {
      assert.doesNotMatch(name, /password|code|token|key|path|secret/i, `${command}: ${name}`);
    }
  }
});

test('GIVEN a host reply WHEN a call resolves THEN the page gets it unchanged', async () => {
  mockIPC(() => ({ enterSends: false }));
  assert.deepEqual(await bridge.accountSettings(), { enterSends: false });
  mockIPC(() => {
    throw 'locked';
  });
  await assert.rejects(bridge.setAccountSettings({ enterSends: true }), (reason) => reason === 'locked');
});


test('GIVEN a core rejection WHEN navigating or changing pins THEN preserve the error', async () => {
  for (const call of [() => bridge.jumpToDate('!r:s', 123), () => bridge.eventContext('!r:s', '$e'), () => bridge.setPinned('!r:s', '$e', true)]) {
    mockIPC(() => Promise.reject('rejected'));
    await assert.rejects(call, (error) => error === 'rejected');
  }
});
