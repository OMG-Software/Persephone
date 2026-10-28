import assert from 'node:assert/strict';
import { test } from 'node:test';
import { bodyParts, MAX_LINKS } from '../src/links.ts';

test('GIVEN text with links WHEN split THEN links keep their exact text without sentence punctuation', () => {
  assert.deepEqual(bodyParts('See https://example.org/a?b=1, then (http://x.test/y).'), [
    { text: 'See ' },
    { link: 'https://example.org/a?b=1' },
    { text: ', then (' },
    { link: 'http://x.test/y' },
    { text: ').' }
  ]);
  assert.deepEqual(bodyParts('no links here'), [{ text: 'no links here' }]);
  assert.deepEqual(bodyParts(''), []);
});

test('GIVEN things that are not web links WHEN split THEN they stay text', () => {
  for (const body of [
    'javascript:alert(1)',
    'file:///C:/x',
    'mxc://example.org/abc',
    'https://',
    'https:///x',
    'ftp://example.org'
  ]) {
    assert.ok(
      bodyParts(body).every((part) => 'text' in part),
      body
    );
  }
});

test('GIVEN a hostile body WHEN split THEN quotes and angle brackets end a link, and links are capped', () => {
  assert.deepEqual(bodyParts('https://a.test/"onclick=x <b>'), [
    { link: 'https://a.test/' },
    { text: '"onclick=x <b>' }
  ]);
  const many = Array.from({ length: MAX_LINKS + 5 }, (_, n) => `https://h${n}.test`).join(' ');
  assert.equal(bodyParts(many).filter((part) => 'link' in part).length, MAX_LINKS);
});
