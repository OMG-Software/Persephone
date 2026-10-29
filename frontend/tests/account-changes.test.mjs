import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as account from '../src/accountChanges.ts';

test('GIVEN each account-change code WHEN shown THEN it has fixed text, and anything else is generic', () => {
  for (const [code, text] of Object.entries(account.accountChangeText)) {
    assert.equal(account.changeText(code), text);
    assert.ok(text.endsWith('.'), code);
  }
  assert.equal(account.changeText('M_FORBIDDEN'), account.accountChangeText.failed);
  assert.equal(account.changeText(undefined), account.accountChangeText.failed);
  assert.match(account.changeText('medium_not_supported'), /identity server/);
});

test('GIVEN refusals from the host WHEN shown THEN each is fixed text', () => {
  assert.match(account.refusalText('limit_exceeded'), /still running/);
  assert.match(account.refusalText('locked'), /locked/);
  assert.match(account.refusalText('core_unavailable'), /could not be reached/);
  assert.match(account.refusalText('anything'), /could not be reached/);
});

test('GIVEN email addresses WHEN checked THEN only plain ones pass', () => {
  for (const ok of ['a@example.org', 'a.b+c@mail.example.co.uk']) {
    assert.ok(account.plausibleEmail(ok), ok);
  }
  for (const bad of ['', 'a', '@example.org', 'a@b', 'a@b@example.org', 'a b@example.org', 'caf\u00e9@example.org', 'a@.org', `${'a'.repeat(250)}@example.org`]) {
    assert.ok(!account.plausibleEmail(bad), bad);
  }
});

test('GIVEN a phone number WHEN read THEN separators drop, and a bad country or number is refused', () => {
  assert.deepEqual(account.phoneNumber(' gb ', '07700 900-123'), { country: 'GB', digits: '07700900123' });
  assert.deepEqual(account.phoneNumber('US', '(555) 010.0199'), { country: 'US', digits: '5550100199' });
  for (const [country, number] of [
    ['G', '07700900123'],
    ['GBR', '07700900123'],
    ['G1', '07700900123'],
    ['GB', '+447700900123'],
    ['GB', '123'],
    ['GB', '0770090012x'],
    ['GB', '1'.repeat(33)]
  ]) {
    assert.equal(account.phoneNumber(country, number), null, `${country} ${number}`);
  }
});

test('GIVEN the notification levels WHEN described THEN custom says another app set it', () => {
  assert.deepEqual(Object.keys(account.notificationLevelText).sort(), ['all', 'custom', 'mentions', 'none']);
  assert.match(account.notificationLevelText.custom, /another app/);
});
