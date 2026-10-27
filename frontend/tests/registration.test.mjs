import assert from 'node:assert/strict';
import { test } from 'node:test';
import { MAX_USERNAME, normalizeUsername, usernameProblem } from '../src/registration.ts';

test('GIVEN typed usernames WHEN normalized THEN they are trimmed, lowercased and lose a leading @', () => {
  assert.equal(normalizeUsername('  @Alice.B '), 'alice.b');
  assert.equal(normalizeUsername('bob'), 'bob');
  assert.equal(normalizeUsername('@@x'), '@x', 'only one leading @ goes');
});

test('GIVEN usernames WHEN checked THEN only localparts of the allowed characters pass', () => {
  for (const valid of ['', 'al', 'a.b_c=d-e/f+g', '0', 'a'.repeat(MAX_USERNAME)]) {
    assert.equal(usernameProblem(valid), null, valid);
  }
  assert.equal(usernameProblem('al:example.org'), 'server');
  assert.equal(usernameProblem('a'.repeat(MAX_USERNAME + 1)), 'too_long');
  for (const invalid of ['a b', 'Al', '@x', 'café', 'a\u202eb', '<b>']) {
    assert.equal(usernameProblem(invalid), 'characters', invalid);
  }
});
