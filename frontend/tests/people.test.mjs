import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let people;

before(async () => {
  vite = await createServer({
    configFile: './vite.config.js',
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  people = await vite.ssrLoadModule('/src/people.ts');
});

after(async () => {
  await vite?.close();
});

test('GIVEN a Matrix user ID WHEN shown as an avatar THEN it gets one or two letters', () => {
  const { initials, localpart } = people;
  assert.equal(localpart('@alex.moreno:persephone.chat'), 'alex.moreno');
  assert.equal(localpart('not an id'), 'not an id');
  assert.equal(initials('@alex.moreno:persephone.chat'), 'AM');
  assert.equal(initials('@mira:matrix.org'), 'M');
  assert.equal(initials('@jonas_k_weber:example.invalid'), 'JW');
  assert.equal(initials('@élodie:example.invalid'), 'É');
});

test('GIVEN hostile characters in a user ID WHEN shown as an avatar THEN none reach it', () => {
  const { initials } = people;
  // Bidi overrides, zero-width and markup characters are skipped.
  assert.equal(initials('@‮evil:example.invalid'), 'E');
  assert.equal(initials('@​<img>:example.invalid'), 'I');
  assert.equal(initials('@<>:example.invalid'), '?');
});

test('avatar colours are stable and one of six', () => {
  const { avatarColour } = people;
  const ids = ['@a:x', '@b:x', '@alex:persephone.chat', '@mira:matrix.org', ''];
  for (const id of ids) {
    assert.match(avatarColour(id), /^c[0-5]$/);
    assert.equal(avatarColour(id), avatarColour(id));
  }
});

test('isolate wraps text in first-strong isolate marks', () => {
  assert.equal(people.isolate('‮abc'), '⁨‮abc⁩');
});
