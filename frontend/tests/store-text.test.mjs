import assert from 'node:assert/strict';
import { test } from 'node:test';
import { resettable, storeText, unlockable } from '../src/storeText.ts';
import { healthLabels } from '../src/healthLabels.ts';

const linuxReasons = ['no_session_bus', 'no_secret_service', 'no_default_keyring', 'no_display', 'keyring_busy'];

test('GIVEN each Linux vault reason WHEN the profile stays locked THEN its own text says what to do next', () => {
  const texts = linuxReasons.map((state) => storeText[state]);
  assert.equal(new Set(texts).size, linuxReasons.length, 'each reason has its own text');
  for (const [index, state] of linuxReasons.entries()) {
    assert.match(texts[index], /^Local store locked: /, state);
    assert.match(texts[index], /choose Unlock/, state);
    assert.notEqual(texts[index], storeText.unsupported, state);
  }
  assert.match(storeText.no_default_keyring, /Create a keyring in Passwords and Keys and make it the default/);
  assert.match(storeText.no_default_keyring, /answer Yes to let Persephone ask the keyring to create one/);
  assert.match(storeText.no_secret_service, /GNOME Keyring or KWallet/);
  assert.match(storeText.no_session_bus, /no session bus/);
  assert.match(storeText.no_display, /could not open its own window/);
  assert.match(storeText.keyring_busy, /another Persephone window is setting up the keyring/);
});

test('GIVEN a Linux vault reason WHEN the locked page shows THEN Unlock tries again and Reset is not offered', () => {
  for (const state of linuxReasons) {
    assert.ok(unlockable.includes(state), state);
    assert.ok(!resettable.includes(state), state);
  }
  assert.ok(!unlockable.includes('unsupported'), 'no vault on this system: nothing to retry');
});

test('GIVEN a Linux vault reason WHEN the health details show THEN a fixed short label is used', () => {
  const label = (storeState) =>
    healthLabels({ accountState: 'signed_out', storeState, syncMode: 'unknown', lastError: null }).store;
  assert.equal(label('no_session_bus'), 'No session bus');
  assert.equal(label('no_secret_service'), 'No keyring service');
  assert.equal(label('no_default_keyring'), 'No default keyring');
  assert.equal(label('no_display'), 'No display for the core windows');
  assert.equal(label('keyring_busy'), 'Keyring busy in another window');
});
