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
    [() => bridge.roomActivity('!r:s', '$e'), 'room_activity', { roomId: '!r:s', eventId: '$e' }],
    [() => bridge.roomActivity('!r:s', null), 'room_activity', { roomId: '!r:s', eventId: null }],
    [() => bridge.setTyping('!r:s', true), 'set_typing', { roomId: '!r:s', typing: true }],
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
    [
      () =>
        bridge.setAccountSettings({
          enterSends: false,
          chatTextPercent: 120,
          interfaceTextPercent: 90,
          desktopNotifications: true,
          sendTyping: true,
          sendReadReceipts: false
        }),
      'set_account_settings',
      {
        settings: {
          enterSends: false,
          chatTextPercent: 120,
          interfaceTextPercent: 90,
          desktopNotifications: true,
          sendTyping: true,
          sendReadReceipts: false
        }
      }
    ],
    [() => bridge.changePassword(true), 'change_password', { signOutOthers: true }],
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
      sendReadReceipts: false
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
