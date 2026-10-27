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
    [() => bridge.setDisplayName('Al'), 'set_display_name', { name: 'Al' }],
    [() => bridge.deviceList(), 'device_list', {}],
    [() => bridge.accountSettings(), 'account_settings', {}],
    [() => bridge.setAccountSettings({ enterSends: false }), 'set_account_settings', { enterSends: false }],
    [() => bridge.profileList(), 'profile_list', {}],
    [() => bridge.addProfile(), 'add_profile', {}],
    [() => bridge.switchProfile(2), 'switch_profile', { slot: 2 }]
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
  const made = await sent(() => bridge.setAccountSettings({ enterSends: true, theme: 'dark' }));
  assert.deepEqual(made.args, { enterSends: true });
});

test('GIVEN a host reply WHEN a call resolves THEN the page gets it unchanged', async () => {
  mockIPC(() => ({ enterSends: false }));
  assert.deepEqual(await bridge.accountSettings(), { enterSends: false });
  mockIPC(() => {
    throw 'locked';
  });
  await assert.rejects(bridge.setAccountSettings({ enterSends: true }), (reason) => reason === 'locked');
});
