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
// The arguments of each host command that are `Option`s.
let optionalArguments;

const camel = (name) => name.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase());

before(async () => {
  const build = await readFile(new URL('../../src-tauri/build.rs', import.meta.url), 'utf8');
  const list = build.slice(build.indexOf('.commands(&['), build.indexOf(']))'));
  registered = new Set([...list.matchAll(/"([a-z_]+)"/g)].map((match) => match[1]));

  // `async fn name(core: State<'_, CoreLink>, room_id: String, ...)`: the
  // arguments the page sends, without Tauri's own State, Window and Channel.
  const host = await readFile(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  hostArguments = new Map();
  optionalArguments = new Map();
  for (const [, name, parameters] of host.matchAll(/(?:async )?fn ([a-z_]+)(?:<[^>]*>)?\(([^)]*)\)/g)) {
    const page = [...parameters.matchAll(/([a-z_]+):\s*([A-Za-z]+)/g)].filter(
      ([, , type]) => !['State', 'Window', 'Webview', 'WebviewWindow', 'AppHandle'].includes(type)
    );
    hostArguments.set(name, page.map(([, parameter]) => camel(parameter)).sort());
    optionalArguments.set(
      name,
      new Set(page.filter(([, , type]) => type === 'Option').map(([, parameter]) => camel(parameter)))
    );
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


// Every timeline switch on, as a new account has them.
const ALL_SHOWN = {
  joins: true,
  leaves: true,
  invitations: true,
  removals: true,
  joinRequests: true,
  displayNames: true,
  profilePictures: true,
  roomName: true,
  topic: true,
  roomPicture: true,
  encryption: true,
  joinRule: true,
  historyVisibility: true,
  permissions: true,
  addresses: true,
  pinnedMessages: true,
  guestAccess: true,
  serverAccess: true,
  creation: true,
  upgrades: true,
  verification: true,
  calls: true,
  callSignalling: true,
  polls: true,
  stickers: true,
  locations: true,
  spaces: true,
  widgets: true,
  otherEvents: true,
  deletedMessages: true
};

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
    [() => bridge.exitApp(), 'exit_app', {}],
    [() => bridge.minimiseToTray(), 'minimise_to_tray', {}],
    [() => bridge.openCallWindow(), 'open_call_window', {}],
    [() => bridge.closeCallWindow(), 'close_call_window', {}],
    [() => bridge.callWindowReady(), 'call_window_ready', {}],
    [() => bridge.callDeviceTest('microphone', null), 'call_device_test', { test: 'microphone', device: null }],
    [() => bridge.callDeviceTest('camera', 'cam-1'), 'call_device_test', { test: 'camera', device: 'cam-1' }],
    [() => bridge.callDeviceTestStop(), 'call_device_test_stop', { id: null }],
    [() => bridge.callDeviceTestStop(4), 'call_device_test_stop', { id: 4 }],
    [() => bridge.callDeviceTestStatus(), 'call_device_test_status', {}],
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
    [() => bridge.readEventsAgain('!r:s', ['$a', '$b']), 'timeline_events', { roomId: '!r:s', eventIds: ['$a', '$b'] }],
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
          notificationNames: false,
          loadRoomPictures: true,
          captureProtection: false,
          timelineEvents: { ...ALL_SHOWN, leaves: false },
          theme: 'red',
          sharePresence: false,
          presence: 'unavailable'
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
          notificationNames: false,
          loadRoomPictures: true,
          captureProtection: false,
          timelineEvents: { ...ALL_SHOWN, leaves: false },
          theme: 'red',
          sharePresence: false,
          presence: 'unavailable'
        }
      }
    ],
    [() => bridge.loadRoomPicture('!r:s'), 'load_room_picture', { roomId: '!r:s' }],
    [() => bridge.changePassword(true), 'change_password', { signOutOthers: true }],
    [() => bridge.signOut(false), 'sign_out', { discardKeys: false }],
    [() => bridge.signOut(true), 'sign_out', { discardKeys: true }],
    [() => bridge.newSessions(), 'new_sessions', {}],
    [() => bridge.thirdPartyModules(), 'third_party_modules', {}],
    [() => bridge.displaySession(), 'display_session', {}],
    [() => bridge.acknowledgeModules(['a.dllunsigned']), 'acknowledge_modules', { ids: ['a.dllunsigned'] }],
    [() => bridge.signInWithOidc('https://e.org'), 'sign_in_with_oidc', { homeserver: 'https://e.org' }],
    [() => bridge.proxySetting(), 'proxy_setting', {}],
    [() => bridge.setProxySetting('127.0.0.1:9050'), 'set_proxy_setting', { proxy: '127.0.0.1:9050' }],
    [() => bridge.setProxySetting(null), 'set_proxy_setting', { proxy: null }],
    [() => bridge.diagnosticsStatus(), 'diagnostics_status', {}],
    [() => bridge.setDiagnostics(true), 'set_diagnostics', { on: true }],
    [() => bridge.setDiagnostics(false), 'set_diagnostics', { on: false }],
    [() => bridge.appMenuStatus(), 'app_menu_status', {}],
    [() => bridge.setAppMenu(true), 'set_app_menu', { on: true }],
    [() => bridge.setAppMenu(false), 'set_app_menu', { on: false }],
    [() => bridge.callDevices(), 'call_devices', {}],
    [() => bridge.setCallDevices('mic', null, 'cam'), 'set_call_devices', { recording: 'mic', playout: null, camera: 'cam' }],
    [() => bridge.setCallDevices(null, 'spk', null), 'set_call_devices', { recording: null, playout: 'spk', camera: null }],
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
    [() => bridge.callReadiness('!r:s'), 'call_readiness', { roomId: '!r:s' }],
    [() => bridge.callJoin('!r:s', false), 'call_join', { roomId: '!r:s', video: false }],
    [() => bridge.callJoin('!r:s', true), 'call_join', { roomId: '!r:s', video: true }],
    [() => bridge.callCamera(true), 'call_camera', { on: true }],
    [() => bridge.callCamera(false), 'call_camera', { on: false }],
    [() => bridge.callScreen(true), 'call_screen', { on: true }],
    [() => bridge.callScreen(false), 'call_screen', { on: false }],
    [() => bridge.callLeave(), 'call_leave', {}],
    [() => bridge.callMute(true), 'call_mute', { muted: true }],
    [() => bridge.callMute(false), 'call_mute', { muted: false }],
    [() => bridge.callDeafen(true), 'call_deafen', { deafened: true }],
    [() => bridge.callDeafen(false), 'call_deafen', { deafened: false }],
    [() => bridge.callVolume(35), 'call_volume', { percent: 35 }],
    [() => bridge.callMicrophone(false, 250), 'call_microphone', { autoLevel: false, gain: 250 }],
    [
      () => bridge.callVideoEffect({ effect: { background: 'sea', blur: 30, filter: 'cool', url: 'https://x' } }),
      'call_video_effect',
      { effect: { background: 'sea', blur: 30, filter: 'cool' } },
      { partial: true }
    ],
    [() => bridge.callVideoEffect({ mirror: false }), 'call_video_effect', { mirror: false }, { partial: true }],
    [
      () => bridge.callVideoEffect({ effect: { background: 'none', blur: 50, filter: 'warm' }, mirror: true }),
      'call_video_effect',
      { effect: { background: 'none', blur: 50, filter: 'warm' }, mirror: true }
    ],
    [() => bridge.callStatus(), 'call_status', {}],
    [() => bridge.callRings(), 'call_rings', {}],
    [() => bridge.callDeclineRing('!r:s'), 'call_decline_ring', { roomId: '!r:s' }],
    [() => bridge.changeRecoveryKey(), 'change_recovery_key', {}],
    [() => bridge.setProfilePassphrase(true), 'set_profile_passphrase', { on: true }],
    [() => bridge.setProfilePassphrase(false), 'set_profile_passphrase', { on: false }],
    [() => bridge.startNewBackup(), 'start_new_backup', {}],
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
  for (const [call, command, args, { partial = false } = {}] of cases) {
    const made = await sent(call);
    assert.equal(made.command, command);
    assert.deepEqual(made.args, args, command);
    assert.ok(registered.has(command), `${command} is registered in build.rs`);
    const keys = Object.keys(args).sort();
    const host = hostArguments.get(command) ?? [];
    if (partial) {
      // A call that sends only some arguments: each is one of the host's,
      // and only an `Option` argument of the host is left out.
      const optional = optionalArguments.get(command) ?? new Set();
      assert.deepEqual(keys.filter((key) => !host.includes(key)), [], `${command} arguments`);
      assert.deepEqual(host.filter((name) => !keys.includes(name) && !optional.has(name)), [], `${command} needs`);
    } else {
      assert.deepEqual(keys, host, `${command} arguments`);
    }
  }
  assert.deepEqual(
    [...optionalArguments.get('call_video_effect')].sort(),
    ['effect', 'mirror'],
    'the effect command takes either part alone'
  );
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
      loadRoomPictures: false,
      captureProtection: true,
      timelineEvents: { ...ALL_SHOWN, polls: false, futureKind: false },
      theme: 'blue',
      sharePresence: true,
      presence: 'offline',
      accent: 'green'
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
      notificationNames: true,
      loadRoomPictures: false,
      captureProtection: true,
      timelineEvents: { ...ALL_SHOWN, polls: false },
      theme: 'blue',
      sharePresence: true,
      presence: 'offline'
    }
  });
});

test('GIVEN the account-change calls WHEN made THEN no password, code, key or path argument exists', async () => {
  const host = await readFile(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  for (const command of [
    'change_password',
    'add_three_pid',
    'change_recovery_key',
    'set_profile_passphrase',
    'start_new_backup',
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

test('GIVEN a pasted image WHEN attached THEN its bytes go as the raw body with the room in a header, to a registered host command', async () => {
  const calls = [];
  mockIPC(() => null);
  window.__TAURI_INTERNALS__.invoke = async (command, args, options) => {
    calls.push({ command, args, options });
    return { kind: 'cancelled' };
  };
  const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
  const reply = await bridge.pasteImage('!r:s', new Blob([bytes], { type: 'image/png' }));
  assert.deepEqual(reply, { kind: 'cancelled' });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, 'paste_image');
  assert.ok(registered.has('paste_image'), 'paste_image is registered in build.rs');
  assert.ok(calls[0].args instanceof Uint8Array, 'a raw body, not JSON');
  assert.deepEqual([...calls[0].args], [...bytes]);
  assert.deepEqual(calls[0].options, { headers: { 'persephone-room-id': '!r:s' } });
  const host = await readFile(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  assert.match(host, /pub const PASTE_ROOM_HEADER: &str = "persephone-room-id";/, 'the header the host reads');
});

test('GIVEN a paste that is not one PNG or JPEG of 1 byte to 20 MiB WHEN attached THEN it is rejected and nothing is read or sent', async () => {
  const sent = [];
  mockIPC(() => null);
  window.__TAURI_INTERNALS__.invoke = async (command) => {
    sent.push(command);
    return null;
  };
  const unread = (type, size) => ({
    type,
    size,
    arrayBuffer() {
      throw new Error('read');
    }
  });
  for (const blob of [
    unread('image/gif', 10),
    unread('image/webp', 10),
    unread('', 10),
    unread('image/png', 0),
    unread('image/jpeg', 20 * 1024 * 1024 + 1)
  ]) {
    await assert.rejects(bridge.pasteImage('!r:s', blob), (reason) => reason === 'rejected', `${blob.type} ${blob.size}`);
  }
  assert.deepEqual(sent, []);
});

test('GIVEN session channel messages WHEN delivered THEN each reaches its own callback and only an unknown message counts as a session change', async () => {
  let channel;
  let levelChannel;
  let othersChannel;
  const asked = [];
  mockIPC((command, args) => {
    asked.push(command);
    if (command === 'diagnostics_status') return { on: false, full: false, failed: false, available: true, path: null };
    assert.equal(command, 'watch_session');
    channel = args.onChange;
    levelChannel = args.onSoundLevel;
    othersChannel = args.onLevels;
    return null;
  });
  const seen = [];
  await bridge.watchSession(
    () => seen.push('session'),
    () => seen.push('rooms'),
    () => seen.push('security'),
    () => seen.push('modules'),
    () => seen.push('calls'),
    () => seen.push('callVideo'),
    () => seen.push('rings'),
    () => seen.push('account'),
    (level) => seen.push(`level ${level}`),
    (levels) => seen.push(`others ${JSON.stringify(levels)}`),
    () => seen.push('deviceTest')
  );
  // The other devices' levels: a bounded list reaches the page, anything
  // else is dropped whole.
  const bob = { userId: '@bob:s', deviceId: 'B', level: 30 };
  for (const value of [
    [bob],
    [],
    [{ ...bob, level: 101 }],
    [{ ...bob, extra: 1 }],
    [{ ...bob, userId: 'x'.repeat(256) }],
    Array(33).fill(bob),
    { 0: bob },
    null
  ]) {
    othersChannel.onmessage(value);
  }
  assert.deepEqual(seen.splice(0), [`others ${JSON.stringify([bob])}`, 'others []']);
  channel.onmessage('deviceTest');
  assert.deepEqual(seen.splice(0), ['deviceTest']);
  // The sound level has its own channel: a number from 0 to 100 reaches the
  // page, anything else is dropped and is never a session change.
  for (const value of [0, 42, 100, 101, -1, 4.5, '42', null]) levelChannel.onmessage(value);
  assert.deepEqual(seen.splice(0), ['level 0', 'level 42', 'level 100']);
  for (const message of ['calls', 'callVideo', 'modules', 'rooms', 'security', 'session', 'something-new', 'calls', 'callvideo', 'rings', 'Rings', 'diagnostics', 'account', 'Account']) {
    channel.onmessage(message);
  }
  assert.deepEqual(seen, ['calls', 'callVideo', 'modules', 'rooms', 'security', 'session', 'session', 'calls', 'session', 'rings', 'session', 'account', 'session']);
  // Watching asks the log's state once, and the `diagnostics` message asks again
  // instead of clearing the page.
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(asked, ['watch_session', 'diagnostics_status', 'diagnostics_status']);
});

test('GIVEN the diagnostic log on WHEN a status read fails THEN the page keeps following it; WHEN a later read says off THEN it stops', async () => {
  let channel;
  let status = { on: true, full: false, failed: false, available: true, path: null };
  mockIPC((command, args) => {
    if (command === 'watch_session') {
      channel = args.onChange;
      return null;
    }
    if (command === 'diagnostics_status') {
      if (status === null) throw 'core_unavailable';
      return status;
    }
    return null;
  });
  const nothing = () => {};
  await bridge.watchSession(nothing, nothing, nothing, nothing, nothing, nothing, nothing, nothing);
  assert.equal(bridge.diagnosticsOn(), true);
  status = null;
  channel.onmessage('diagnostics');
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(bridge.diagnosticsOn(), true, 'an unreadable state is not off');
  status = { on: false, full: false, failed: false, available: true, path: null };
  channel.onmessage('diagnostics');
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(bridge.diagnosticsOn(), false);
});

test('GIVEN the diagnostic log WHEN it is on THEN host calls are noted by name and outcome only, and off notes nothing', async () => {
  const records = [];
  let on = true;
  mockIPC((command, args) => {
    if (command === 'diagnostics_status') return { on, full: false, failed: false, available: true, path: null };
    if (command === 'diagnostic_records') {
      records.push(...args.records);
      return null;
    }
    if (command === 'watch_session') return null;
    if (command === 'set_proxy_setting') throw 'invalid_argument';
    if (command === 'sign_in') throw 'secret: Bearer abc';
    return null;
  });
  const nothing = () => {};
  await bridge.watchSession(nothing, nothing, nothing, nothing, nothing, nothing, nothing, nothing);
  assert.equal(bridge.diagnosticsOn(), true);
  await bridge.proxySetting();
  await assert.rejects(bridge.setProxySetting('127.0.0.1:1'));
  await assert.rejects(bridge.signIn('https://e.org'));
  const diagnostics = await import('../src/bridge/diagnostics.ts');
  const batch = diagnostics.takeBatch();
  const messages = batch.map((record) => record.message.replace(/ \d+ ms$/, ''));
  assert.deepEqual(messages, ['proxy_setting ok', 'set_proxy_setting invalid_argument', 'sign_in error']);
  const text = JSON.stringify(batch);
  for (const secret of ['127.0.0.1', 'e.org', 'Bearer', 'abc']) assert.ok(!text.includes(secret), secret);

  diagnostics.setPageDiagnostics(false);
  await bridge.proxySetting();
  assert.deepEqual(diagnostics.takeBatch(), []);
  assert.deepEqual(records, []);
});

// The page as `watch_session` left it: `page` is the host's reply.
async function watchAs(page) {
  mockIPC((command) => {
    if (command === 'diagnostics_status') return { on: false, full: false, failed: false, available: true, path: null };
    return command === 'watch_session' ? page : null;
  });
  const nothing = () => {};
  await bridge.watchSession(nothing, nothing, nothing, nothing, nothing, nothing, nothing, nothing);
}

test('GIVEN the technical details WHEN turned on and off THEN the host is asked with a channel and the page count, readings in the contract reach the page and any other is dropped', async () => {
  await watchAs(4);
  const calls = [];
  mockIPC((command, args) => {
    calls.push({ command, args });
    return null;
  });
  const seen = [];
  await bridge.startCallStats((stats) => seen.push(stats));
  assert.equal(calls[0].command, 'call_stats');
  assert.equal(calls[0].args.on, true);
  assert.equal(calls[0].args.page, 4);
  const reading = { microphone: null, camera: null, screen: null, participants: [], connections: [] };
  for (const value of [reading, { ...reading, extra: 1 }, null, 'x', { ...reading, connections: [{ connection: 9 }] }]) {
    calls[0].args.onStats.onmessage(value);
  }
  assert.deepEqual(seen, [reading]);
  // Each message, handled or dropped, tells the host to send the next.
  const reads = calls.filter((call) => call.command === 'call_stats_read');
  assert.equal(reads.length, 5);
  assert.ok(reads.every((call) => call.args.page === 4 && Object.keys(call.args).length === 1));
  assert.ok(registered.has('call_stats_read'));
  assert.deepEqual(hostArguments.get('call_stats_read'), ['page']);
  await bridge.stopCallStats();
  const stop = calls[calls.length - 1];
  assert.equal(stop.command, 'call_stats');
  assert.equal(stop.args.on, false);
  assert.equal(stop.args.page, 4);
  // The host registers the command with exactly these arguments.
  assert.ok(registered.has('call_stats'));
  assert.deepEqual(hostArguments.get('call_stats'), ['on', 'onStats', 'page']);
  mockIPC(() => Promise.reject('unavailable'));
  await assert.rejects(bridge.startCallStats(() => {}), (reason) => reason === 'unavailable');
});

test('GIVEN a stale page count WHEN the host refuses the read of a reading THEN the page goes on and the reading still reaches it', async () => {
  await watchAs(8);
  const channels = [];
  mockIPC((command, args) => {
    if (command === 'call_stats_read') return Promise.reject('invalid_argument');
    channels.push(args.onStats);
    return null;
  });
  const seen = [];
  await bridge.startCallStats((stats) => seen.push(stats));
  const reading = { microphone: null, camera: null, screen: null, participants: [], connections: [] };
  channels[0].onmessage(reading);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(seen, [reading]);
});

test('GIVEN no usable page count from the host WHEN the technical details are turned on THEN the start fails and asks nothing, and a stop asks nothing', async () => {
  for (const reply of [null, 0, -1, 1.5, '3', Number.MAX_SAFE_INTEGER + 2]) {
    await watchAs(reply);
    const asked = [];
    mockIPC((command) => {
      asked.push(command);
      return null;
    });
    await assert.rejects(bridge.startCallStats(() => {}), (reason) => reason === 'core_unavailable');
    await bridge.stopCallStats();
    assert.deepEqual(asked, [], String(reply));
  }
});

test('GIVEN a statistics start still at the host WHEN the panel closes and opens again THEN the host gets start, stop, start in that order', async () => {
  await watchAs(5);
  const sent = [];
  let release;
  const first = new Promise((resolve) => (release = resolve));
  mockIPC((command, args) => {
    sent.push(args.on);
    return sent.length === 1 ? first.then(() => null) : null;
  });
  const startA = bridge.startCallStats(() => {});
  const stopA = bridge.stopCallStats();
  const startB = bridge.startCallStats(() => {});
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(sent, [true], 'nothing else goes while the first start runs');
  release();
  await Promise.all([startA, stopA, startB]);
  // The late stop of the closed panel comes before the new start, so the
  // readings stay on for the panel that is open.
  assert.deepEqual(sent, [true, false, true]);
});

test('GIVEN a statistics start that waits for its turn WHEN a reload changes the page count meanwhile THEN the start and the reads of its readings both use the new count', async () => {
  await watchAs(5);
  let page = 5;
  const calls = [];
  let release;
  const first = new Promise((resolve) => (release = resolve));
  mockIPC((command, args) => {
    if (command === 'diagnostics_status') return { on: false, full: false, failed: false, available: true, path: null };
    if (command === 'watch_session') return page;
    calls.push({ command, args });
    return calls.length === 1 ? first.then(() => null) : null;
  });
  const startA = bridge.startCallStats(() => {});
  const startB = bridge.startCallStats(() => {});
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(
    calls.map((call) => call.args.page),
    [5],
    'the first start is at the host, the second waits for its turn'
  );
  page = 9;
  const nothing = () => {};
  await bridge.watchSession(nothing, nothing, nothing, nothing, nothing, nothing, nothing, nothing);
  release();
  await Promise.all([startA, startB]);
  const starts = calls.filter((call) => call.command === 'call_stats');
  assert.deepEqual(
    starts.map((call) => call.args.page),
    [5, 9]
  );
  const reading = { microphone: null, camera: null, screen: null, participants: [], connections: [] };
  starts[1].args.onStats.onmessage(reading);
  const reads = calls.filter((call) => call.command === 'call_stats_read');
  assert.deepEqual(
    reads.map((call) => call.args.page),
    [9],
    'the reading is acknowledged with the count its channel is kept under'
  );
});

test('GIVEN a statistics start that fails WHEN a stop follows THEN the stop is still sent', async () => {
  await watchAs(6);
  const sent = [];
  mockIPC((command, args) => {
    sent.push(args.on);
    return args.on ? Promise.reject('locked') : null;
  });
  await assert.rejects(bridge.startCallStats(() => {}), (reason) => reason === 'locked');
  await bridge.stopCallStats();
  assert.deepEqual(sent, [true, false]);
});

test('GIVEN the video frame call WHEN the host answers THEN an empty body is no picture, a whole picture is parsed and anything else is refused', async () => {
  const body = new Uint8Array(20 + 8);
  const prefix = new DataView(body.buffer);
  prefix.setBigUint64(0, 12n, true);
  prefix.setUint32(8, 1, true);
  prefix.setUint32(12, 2, true);
  prefix.setUint32(16, 1, true);

  const calls = [];
  const answer = (reply) => {
    mockIPC((command, args) => {
      calls.push({ command, args });
      return reply;
    });
  };
  answer(new ArrayBuffer(0));
  assert.equal(await bridge.callVideoFrame(3, 11, 2), null);
  answer(body.buffer);
  const picture = await bridge.callVideoFrame(0, 0, 0);
  assert.deepEqual([picture.seq, picture.rotation, picture.width, picture.height], [12, 1, 2, 1]);
  assert.deepEqual(calls.map((call) => call.command), ['call_video_frame', 'call_video_frame']);
  assert.deepEqual(calls[0].args, { tile: 3, after: 11, stream: 2 });
  assert.ok(registered.has('call_video_frame') && registered.has('call_camera'));
  assert.deepEqual(hostArguments.get('call_video_frame'), ['after', 'stream', 'tile']);
  assert.deepEqual(hostArguments.get('call_camera'), ['on']);
  // The page names no screen: the screen command takes the flag and nothing else.
  assert.ok(registered.has('call_screen'));
  assert.deepEqual(hostArguments.get('call_screen'), ['on']);
  assert.deepEqual(hostArguments.get('call_join'), ['roomId', 'video']);
  assert.ok(registered.has('call_rings') && registered.has('call_decline_ring'));
  assert.deepEqual(hostArguments.get('call_rings'), []);
  assert.deepEqual(hostArguments.get('call_decline_ring'), ['roomId']);
  assert.deepEqual(hostArguments.get('set_call_devices'), ['camera', 'playout', 'recording']);

  for (const bad of [null, { seq: 1 }, 'x', body.slice(0, 27), new Uint8Array(21)]) {
    answer(bad);
    await assert.rejects(bridge.callVideoFrame(0, 0, 0), (reason) => reason === 'bad_video');
  }
  mockIPC(() => Promise.reject('limit_exceeded'));
  await assert.rejects(bridge.callVideoFrame(0, 0, 0), (reason) => reason === 'limit_exceeded');
});
