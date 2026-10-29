import assert from 'node:assert/strict';
import { test } from 'node:test';
import { OwnNameKeeper } from '../src/ownName.ts';

// A keeper whose reads wait until the test answers them, in any order.
function keeper() {
  const reads = [];
  const shown = { name: 'unset' };
  const kept = new OwnNameKeeper(
    () =>
      new Promise((resolve, reject) => {
        reads.push({ resolve, reject });
      }),
    (name) => {
      shown.name = name;
    }
  );
  return { kept, reads, shown };
}

const ALICE = '@alice:example.invalid';
const BOB = '@bob:example.invalid';

test('GIVEN a signed-in account WHEN its name is read THEN it is shown', async () => {
  const { kept, reads, shown } = keeper();
  const reading = kept.account(ALICE);
  assert.equal(shown.name, null, 'no name until it is read');
  reads[0].resolve({ displayName: 'Alice' });
  await reading;
  assert.equal(shown.name, 'Alice');
});

test('GIVEN a read still running WHEN a save sets a name THEN the older read does not replace it', async () => {
  const { kept, reads, shown } = keeper();
  const reading = kept.account(ALICE);
  kept.saved(ALICE, 'Alice Smith');
  reads[0].resolve({ displayName: null });
  await reading;
  assert.equal(shown.name, 'Alice Smith');
});

test('GIVEN a save for one account WHEN it ends after a switch THEN the new account keeps its own name', async () => {
  const { kept, reads, shown } = keeper();
  kept.account(ALICE);
  const reading = kept.account(BOB);
  reads[1].resolve({ displayName: 'Bob' });
  await reading;
  kept.saved(ALICE, 'Alice Smith');
  assert.equal(shown.name, 'Bob');
});

test('GIVEN a read for one account WHEN it answers after a switch THEN it is ignored', async () => {
  const { kept, reads, shown } = keeper();
  const first = kept.account(ALICE);
  const second = kept.account(BOB);
  reads[1].resolve({ displayName: 'Bob' });
  reads[0].resolve({ displayName: 'Alice' });
  await Promise.all([first, second]);
  assert.equal(shown.name, 'Bob');
});

test('GIVEN no account WHEN signed out THEN no name shows and nothing is read', async () => {
  const { kept, reads, shown } = keeper();
  const reading = kept.account(ALICE);
  await kept.account(null);
  assert.equal(shown.name, null);
  assert.equal(reads.length, 1);
  reads[0].resolve({ displayName: 'Alice' });
  await reading;
  assert.equal(shown.name, null, 'a read from before the sign-out changes nothing');
  kept.saved(ALICE, 'Alice');
  assert.equal(shown.name, null, 'a save from before the sign-out changes nothing');
});

test('GIVEN a failed read WHEN it answers THEN no name shows', async () => {
  const { kept, reads, shown } = keeper();
  const reading = kept.account(ALICE);
  reads[0].reject('core_unavailable');
  await reading;
  assert.equal(shown.name, null);
});
