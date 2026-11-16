import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as modules from '../src/modules.ts';

const found = (fileName, overrides = {}) => ({
  id: `${fileName.toLowerCase()}\u001funsigned`,
  fileName,
  path: `C:\Tools\${fileName}`,
  signer: null,
  signature: 'unsigned',
  processes: ['core'],
  firstSeen: 1_700_000_000_000,
  acknowledged: false,
  ...overrides
});
const report = (list, state = 'ready') => ({ state, scannedAt: 1, modules: list, truncated: false });

test('GIVEN signatures WHEN labelled THEN a missing signer says why', () => {
  assert.equal(modules.signerLabel(found('a.dll')), 'Unsigned');
  assert.equal(modules.signerLabel(found('a.dll', { signature: 'invalid' })), 'Signature not valid');
  assert.equal(
    modules.signerLabel(found('a.dll', { signature: 'valid', signer: 'Sophos Ltd' })),
    'Sophos Ltd'
  );
  assert.equal(modules.signerLabel(found('a.dll', { signature: 'valid' })), 'Signed');
});

test('GIVEN processes WHEN labelled THEN each has the name the user sees', () => {
  assert.equal(modules.processLabel('core'), 'Persephone core');
  assert.equal(modules.processLabel('ui'), 'Window');
  assert.equal(modules.processLabel('web_view'), 'Web view');
  assert.equal(modules.processesLabel(['core', 'web_view']), 'Persephone core, Web view');
});

test('GIVEN one or three unreviewed modules WHEN the notice is made THEN it names the first and counts the rest', () => {
  const one = report([found('hook.dll'), found('old.dll', { acknowledged: true })]);
  assert.equal(modules.noticeText(one), 'Other code is loaded into Persephone: hook.dll (Unsigned)');
  const three = report([
    found('hmpalert.dll', { signature: 'valid', signer: 'Sophos Ltd' }),
    found('b.dll', { signature: 'invalid' }),
    found('c.dll')
  ]);
  assert.equal(
    modules.noticeText(three),
    'Other code is loaded into Persephone: hmpalert.dll (Sophos Ltd) and 2 more'
  );
});

test('GIVEN no report or only reviewed modules WHEN the notice is made THEN there is none', () => {
  assert.equal(modules.noticeText(null), null);
  assert.equal(modules.noticeText(report([])), null);
  assert.equal(modules.noticeText(report([found('a.dll', { acknowledged: true })])), null);
});

test('GIVEN scan states WHEN their text is made THEN only a list-less state has text', () => {
  assert.equal(modules.stateText(report([], 'pending')), 'Checking…');
  assert.equal(modules.stateText(report([], 'unsupported')), 'Not available on this system.');
  assert.equal(modules.stateText(report([])), 'No other code is loaded.');
  assert.equal(modules.stateText(report([found('a.dll')])), null);
});

test('GIVEN unreviewed modules WHEN ids are chosen THEN reviewed and out-of-limit ids are left out', () => {
  const list = [
    found('a.dll'),
    found('b.dll', { acknowledged: true }),
    found('c.dll', { id: 'x'.repeat(513) }),
    found('d.dll', { id: '' })
  ];
  assert.deepEqual(modules.acknowledgeIds(modules.unreviewed(report(list))), [list[0].id]);
  const many = Array.from({ length: 130 }, (_, n) => found(`m${n}.dll`));
  assert.equal(modules.acknowledgeIds(many).length, 128);
});
