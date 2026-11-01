import assert from 'node:assert/strict';
import { test } from 'node:test';
import { loadIgnoredUsers } from '../src/ignoredUsers.ts';

test('GIVEN a failed ignored-list load WHEN a retry succeeds THEN the list is shown and the old error is cleared', async () => {
  let state = { users: null, failure: null };
  let calls = 0;
  const read = async () => {
    calls++;
    if (calls === 1) throw new Error('unavailable');
    return { users: ['@ignored:example.org'], truncated: false };
  };
  const publish = (result) => { state = result; };

  await loadIgnoredUsers(read, () => true, publish);
  assert.equal(state.users, null);
  assert.equal(state.failure, 'The ignored-user list could not be loaded.');

  await loadIgnoredUsers(read, () => true, publish);
  assert.equal(calls, 2);
  assert.deepEqual(state.users, { users: ['@ignored:example.org'], truncated: false });
  assert.equal(state.failure, null);
});

test('GIVEN a closed ignored-list dialog WHEN a pending read succeeds or fails THEN neither result updates its state', async () => {
  for (const fails of [false, true]) {
    let active = true;
    let settle;
    const read = () => new Promise((resolve, reject) => {
      settle = () => fails ? reject(new Error('unavailable')) : resolve({ users: [], truncated: false });
    });
    const updates = [];
    const pending = loadIgnoredUsers(read, () => active, (state) => updates.push(state));
    active = false;
    settle();
    await pending;
    assert.deepEqual(updates, []);
  }
});
