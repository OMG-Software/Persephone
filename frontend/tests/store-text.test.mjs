import assert from 'node:assert/strict';
import { test } from 'node:test';
import { resettable, storeText, unlockable } from '../src/storeText.ts';
import { healthLabels } from '../src/healthLabels.ts';

const linuxReasons = ['no_session_bus', 'no_secret_service', 'no_default_keyring', 'no_display', 'keyring_busy'];
// Reasons the user can fix while Persephone runs. A missing display is not
// one: the core's window thread starts once and does not try again.
const retryable = linuxReasons.filter((state) => state !== 'no_display');

test('GIVEN each Linux vault reason WHEN the profile stays locked THEN its own text says what to do next', () => {
  const texts = linuxReasons.map((state) => storeText[state]);
  assert.equal(new Set(texts).size, linuxReasons.length, 'each reason has its own text');
  for (const [index, state] of linuxReasons.entries()) {
    assert.match(texts[index], /^Local store locked: /, state);
    assert.notEqual(texts[index], storeText.unsupported, state);
  }
  for (const state of retryable) {
    assert.match(storeText[state], /choose Unlock/, state);
  }
  assert.match(storeText.no_default_keyring, /Create a keyring in Passwords and Keys and make it the default/);
  assert.match(storeText.no_default_keyring, /answer Yes to let Persephone ask the keyring to create one/);
  assert.match(storeText.no_secret_service, /GNOME Keyring or KWallet/);
  assert.match(storeText.no_session_bus, /no session bus/);
  assert.match(storeText.keyring_busy, /another Persephone window is creating the keyring item/);
  assert.match(storeText.keyring_busy, /Let it finish/);
});

test('GIVEN a Linux vault reason the user can fix WHEN the locked page shows THEN Unlock tries again and Reset is not offered', () => {
  for (const state of retryable) {
    assert.ok(unlockable.includes(state), state);
    assert.ok(!resettable.includes(state), state);
  }
  assert.ok(!unlockable.includes('unsupported'), 'no vault on this system: nothing to retry');
});

test('GIVEN no display for the core windows WHEN the locked page shows THEN it says to restart and offers neither Unlock nor Reset', () => {
  assert.match(storeText.no_display, /could not open its own window/);
  assert.match(storeText.no_display, /Quit Persephone and start it again in a desktop session with a display/);
  assert.doesNotMatch(storeText.no_display, /Unlock/);
  assert.ok(!unlockable.includes('no_display'), 'the window thread does not start again');
  assert.ok(!resettable.includes('no_display'));
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
