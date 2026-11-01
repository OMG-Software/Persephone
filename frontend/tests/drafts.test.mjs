import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DraftKeeper, EMPTY_DRAFT, DRAFT_SAVE_FAILED, MAX_DRAFT_BYTES } from '../src/drafts.ts';
import { discardInactiveDraft } from '../src/draftManagement.ts';

const turn = () => new Promise(setImmediate);
const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const draft = (body, replyTo = null) => ({ body, replyTo });

test('GIVEN saved-draft management WHEN discarding THEN the selected editor is protected and inactive draft removal clears its reply', async () => {
  const saves = [];
  const save = async (...args) => { saves.push(args); };
  assert.equal(await discardInactiveDraft('!active:s', () => '!active:s', save), false);
  assert.deepEqual(saves, []);
  assert.equal(await discardInactiveDraft('!left:s', () => '!active:s', save), true);
  assert.deepEqual(saves, [['!left:s', EMPTY_DRAFT]]);
  await assert.rejects(discardInactiveDraft('!left:s', () => null, async () => { throw Error('locked'); }), /locked/);
});

test('GIVEN rapid edits WHEN 350 ms passes THEN one latest room-bound draft is saved', async (context) => {
  context.mock.timers.enable({ apis: ['setTimeout'] });
  const saves = [];
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async (...args) => { saves.push(args); } }, () => {});
  await keeper.select('!a:s');
  keeper.update(draft('a'));
  keeper.update(draft('newest', '$reply'));
  context.mock.timers.tick(349);
  await turn();
  assert.deepEqual(saves, []);
  context.mock.timers.tick(1);
  await turn();
  assert.deepEqual(saves, [['!a:s', draft('newest', '$reply')]]);
  keeper.clear();
});

test('GIVEN an unfinished restore WHEN editing is attempted THEN composing waits for the saved draft', async () => {
  const first = deferred();
  const shown = [];
  const keeper = new DraftKeeper({ load: async (room) => room === '!a:s' ? first.promise : draft('room B'), save: async () => {} }, (value) => shown.push(value));
  const restoring = keeper.select('!a:s');
  await turn();
  assert.equal(keeper.update(draft('typed before restore')), false);
  first.resolve(draft('old saved text'));
  await restoring;
  assert.deepEqual(shown.at(-1), draft('old saved text'));
  assert.equal(keeper.update(draft('new edit after restore')), true);
  await keeper.flush();
  const next = keeper.select('!b:s');
  assert.deepEqual(shown.at(-1), EMPTY_DRAFT);
  await next;
  assert.deepEqual(shown.at(-1), draft('room B'));
  keeper.clear();
});

test('GIVEN late room and profile results WHEN selection changes or clears THEN plaintext cannot return', async () => {
  const a = deferred();
  const b = deferred();
  const shown = [];
  const keeper = new DraftKeeper({ load: (room) => room === '!a:s' ? a.promise : b.promise, save: async () => {} }, (value) => shown.push(value));
  const first = keeper.select('!a:s');
  await turn();
  const second = keeper.select('!b:s');
  await turn();
  a.resolve(draft('account A old room'));
  await first;
  assert.deepEqual(shown.at(-1), EMPTY_DRAFT);
  keeper.clear();
  b.resolve(draft('account A room B'));
  await second;
  assert.deepEqual(shown.at(-1), EMPTY_DRAFT);
  assert.ok(!shown.some((value) => value.body.startsWith('account A')));
});

test('GIVEN a failed restore WHEN autosave or flush runs THEN it never writes an empty replacement', async (context) => {
  context.mock.timers.enable({ apis: ['setTimeout'] });
  let fail = true;
  const saves = [];
  const shown = [];
  const keeper = new DraftKeeper({
    load: async () => { if (fail) throw Error('unavailable'); return draft('existing encrypted draft'); },
    save: async (...args) => { saves.push(args); }
  }, (value, failure) => shown.push({ value, failure }));
  await keeper.select('!a:s');
  context.mock.timers.tick(1000);
  await keeper.flush();
  assert.deepEqual(saves, []);
  assert.match(shown.at(-1).failure, /could not be loaded/);
  assert.equal(keeper.update(draft('must not replace unseen saved text')), false);
  context.mock.timers.tick(1000);
  await keeper.flush();
  assert.deepEqual(saves, []);
  fail = false;
  assert.equal(await keeper.retry(), true);
  assert.deepEqual(shown.at(-1).value, draft('existing encrypted draft'));
  assert.deepEqual(saves, []);
  keeper.clear();
});

test('GIVEN an unread draft WHEN explicitly discarded THEN composing resumes only after the empty replacement saves', async () => {
  let fail = true;
  const saves = [];
  let ready = false;
  const keeper = new DraftKeeper({ load: async () => { throw Error('read failed'); }, save: async (...args) => {
    if (fail) throw Error('save failed');
    saves.push(args);
  } }, (_value, _failure, nextReady) => { ready = nextReady; });
  await keeper.select('!a:s');
  assert.equal(ready, false);
  assert.equal(await keeper.discard(), false);
  assert.equal(ready, false);
  assert.equal(keeper.update(draft('still blocked')), false);
  fail = false;
  assert.equal(await keeper.discard(), true);
  assert.equal(ready, true);
  assert.deepEqual(saves, [['!a:s', EMPTY_DRAFT]]);
  assert.equal(keeper.update(draft('new message after explicit discard')), true);
  keeper.clear();
});

test('GIVEN overlapping saves WHEN older work finishes THEN writes remain serialized and final text stays latest', async () => {
  const first = deferred();
  let active = 0;
  let maximum = 0;
  const saves = [];
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async (room, value) => {
    active += 1;
    maximum = Math.max(maximum, active);
    saves.push([room, value]);
    if (saves.length === 1) await first.promise;
    active -= 1;
  } }, () => {});
  await keeper.select('!a:s');
  keeper.update(draft('first'));
  const one = keeper.flush();
  await turn();
  keeper.update(draft('second'));
  const two = keeper.flush();
  await turn();
  assert.deepEqual(saves, [['!a:s', draft('first')]]);
  first.resolve();
  assert.deepEqual(await Promise.all([one, two]), [true, true]);
  assert.equal(maximum, 1);
  assert.deepEqual(saves.at(-1), ['!a:s', draft('second')]);
  keeper.clear();
});

test('GIVEN queued old-profile saves WHEN the profile clears THEN unsent work is skipped', async () => {
  const first = deferred();
  const saves = [];
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async (room, value) => {
    saves.push([room, value]);
    if (saves.length === 1) await first.promise;
  } }, () => {});
  await keeper.select('!a:s');
  keeper.update(draft('already in flight'));
  const one = keeper.flush();
  await turn();
  keeper.update(draft('must not reach next profile'));
  const two = keeper.flush();
  keeper.clear();
  first.resolve();
  await Promise.all([one, two]);
  assert.equal(saves.length, 1);
});

test('GIVEN a failed save WHEN retried THEN leaving fails first and preserves the text for retry', async () => {
  let fail = true;
  const shown = [];
  const saves = [];
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async (room, value) => {
    if (fail) throw Error('full');
    saves.push([room, value]);
  } }, (value, failure) => shown.push({ value, failure }));
  await keeper.select('!a:s');
  keeper.update(draft('keep me'));
  assert.equal(await keeper.flush(), false);
  assert.deepEqual(shown.at(-1), { value: draft('keep me'), failure: DRAFT_SAVE_FAILED });
  fail = false;
  assert.equal(await keeper.retry(), true);
  assert.deepEqual(saves, [['!a:s', draft('keep me')]]);
  keeper.clear();
});

test('GIVEN an existing-message edit WHEN the editor changes THEN new-message text and reply stay separate', async () => {
  const saves = [];
  const keeper = new DraftKeeper({ load: async () => draft('new unsent message', '$reply'), save: async (...args) => { saves.push(args); } }, () => {});
  await keeper.select('!a:s');
  keeper.beginEdit();
  keeper.update(draft('replacement for an old event'));
  await keeper.flush();
  assert.deepEqual(saves, []);
  keeper.endEdit();
  keeper.update(draft('new unsent message continued', '$reply'));
  await keeper.flush();
  assert.deepEqual(saves, [['!a:s', draft('new unsent message continued', '$reply')]]);
  keeper.clear();
});

test('GIVEN multibyte text WHEN it exceeds the byte limit THEN it is not saved', async () => {
  const saves = [];
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async (...args) => { saves.push(args); } }, () => {});
  await keeper.select('!a:s');
  keeper.update(draft('😃'.repeat(MAX_DRAFT_BYTES / 4 + 1)));
  assert.equal(await keeper.flush(), false);
  assert.deepEqual(saves, []);
  keeper.update(draft('😃'.repeat(MAX_DRAFT_BYTES / 4)));
  assert.equal(await keeper.flush(), true);
  assert.equal(saves.length, 1);
  keeper.clear();
});


test('GIVEN full draft storage WHEN another saved draft is discarded THEN the unsaved editor can retry without losing text', async () => {
  const stored = new Map([['!old:s', draft('previous room')]]);
  const save = async (room, value) => {
    if (value.body === '' && value.replyTo === null) { stored.delete(room); return; }
    if (!stored.has(room) && stored.size >= 1) throw Error('limit_exceeded');
    stored.set(room, { ...value });
  };
  let visible;
  const keeper = new DraftKeeper({ load: async room => stored.get(room) ?? EMPTY_DRAFT, save }, value => { visible = value; });
  await keeper.select('!current:s');
  keeper.update(draft('keep my unsaved text'));
  assert.equal(await keeper.flush(), false);
  assert.equal(visible.body, 'keep my unsaved text');
  assert.equal(await discardInactiveDraft('!old:s', () => '!current:s', save), true);
  assert.equal(await keeper.retry(), true);
  assert.equal(stored.get('!current:s').body, 'keep my unsaved text');
  keeper.clear();
});


test('GIVEN a stalled draft write WHEN locking THEN key protection starts without waiting for the save', async () => {
  const pendingSave = deferred();
  let saving = false;
  let protectedNow = false;
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async () => {
    saving = true;
    await pendingSave.promise;
  } }, () => {});
  await keeper.select('!a:s');
  keeper.update(draft('pending text'));
  const flush = keeper.flush();
  await turn();
  assert.equal(saving, true);
  const state = await keeper.protect(async () => { protectedNow = true; keeper.clear(); return 'locked'; });
  assert.equal(protectedNow, true);
  assert.equal(state, 'locked');
  pendingSave.resolve();
  await flush;
});

test('GIVEN a refused oversized draft WHEN signing out THEN the local draft cannot block session revocation', async () => {
  const keeper = new DraftKeeper({ load: async () => EMPTY_DRAFT, save: async () => { throw Error('full'); } }, () => {});
  await keeper.select('!a:s');
  keeper.update(draft('x'.repeat(MAX_DRAFT_BYTES + 1)));
  assert.equal(await keeper.flush(), false);
  let revocations = 0;
  assert.equal(await keeper.protect(async () => { revocations++; keeper.clear(); return 'signed_out'; }), 'signed_out');
  assert.equal(revocations, 1);
});
