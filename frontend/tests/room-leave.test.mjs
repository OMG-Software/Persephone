import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DraftKeeper, EMPTY_DRAFT, MAX_DRAFT_BYTES } from '../src/drafts.ts';
import { leaveAfterDraftFlush } from '../src/roomLeave.ts';

test('GIVEN an overlimit or failed draft WHEN Leave is confirmed THEN membership stays and the draft remains recoverable', async () => {
  for (const overlimit of [true, false]) {
    let joined = true;
    let fail = !overlimit;
    const shown = [];
    const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async () => {
      if (!joined) throw Error('not joined');
      if (fail) throw Error('disk full');
    } }, (draft) => shown.push(draft));
    await keeper.select('!leaving:s');
    keeper.update({ body: overlimit ? '😃'.repeat(MAX_DRAFT_BYTES) : 'recoverable text', replyTo: '$reply' });
    const leave = () => leaveAfterDraftFlush(() => keeper.flush(), async () => { joined = false; }, () => true);
    assert.equal(await leave(), false);
    assert.equal(joined, true, 'a failed draft must prevent the homeserver Leave call');
    if (!overlimit) assert.equal(shown.at(-1).body, 'recoverable text');
    fail = false;
    keeper.update({ body: 'recovered text', replyTo: '$reply' });
    assert.equal(await leave(), true);
    assert.equal(joined, false);
    // This is App.leftRoom's release policy. It prevents saving to a left room.
    keeper.clear();
    assert.equal(await keeper.flush(), true);
    await keeper.select('!next:s');
    assert.deepEqual(shown.at(-1), EMPTY_DRAFT);
    keeper.clear();
  }
});

test('GIVEN an in-flight draft save WHEN leaving THEN the save finishes before the room is left', async () => {
  const order = [];
  let finish;
  const saving = new Promise((resolve) => { finish = resolve; });
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async () => {
    order.push('save started');
    await saving;
    order.push('save finished');
  } }, () => {});
  await keeper.select('!r:s');
  keeper.update({ body: 'keep me', replyTo: null });
  const leaving = leaveAfterDraftFlush(() => keeper.flush(), async () => { order.push('left'); }, () => true);
  await new Promise(setImmediate);
  assert.deepEqual(order, ['save started']);
  finish();
  assert.equal(await leaving, true);
  assert.deepEqual(order, ['save started', 'save finished', 'left']);
  keeper.clear();
});

test('GIVEN a profile change while the Leave gate waits THEN no old-room Leave request is sent', async () => {
  let current = true;
  let leaves = 0;
  assert.equal(await leaveAfterDraftFlush(async () => { current = false; return true; }, async () => { leaves += 1; }, () => current), false);
  assert.equal(leaves, 0);
});
