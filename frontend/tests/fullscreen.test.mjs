import assert from 'node:assert/strict';
import { test } from 'node:test';
import { isFullscreen, FullscreenReporter, toggleFullscreen } from '../src/fullscreen.ts';

const fakeDocument = () => {
  const doc = {
    fullscreenElement: null,
    exits: 0,
    async exitFullscreen() {
      doc.exits += 1;
      doc.fullscreenElement = null;
    }
  };
  return doc;
};

const fakeElement = (doc, refuse = false) => {
  const element = {
    requests: 0,
    async requestFullscreen() {
      element.requests += 1;
      if (refuse) throw new TypeError('denied');
      doc.fullscreenElement = element;
    }
  };
  return element;
};

test('GIVEN no full screen element WHEN asked THEN no element is in full screen', () => {
  const doc = fakeDocument();
  assert.equal(isFullscreen(doc, fakeElement(doc)), false);
  assert.equal(isFullscreen(doc, null), false);
  assert.equal(isFullscreen(doc, undefined), false);
});

test('GIVEN an element that is not in full screen WHEN toggled THEN it is requested', async () => {
  const doc = fakeDocument();
  const element = fakeElement(doc);
  await toggleFullscreen(doc, element);
  assert.equal(element.requests, 1);
  assert.equal(doc.exits, 0);
  assert.equal(isFullscreen(doc, element), true);
});

test('GIVEN the element in full screen WHEN toggled THEN the document leaves full screen', async () => {
  const doc = fakeDocument();
  const element = fakeElement(doc);
  await toggleFullscreen(doc, element);
  await toggleFullscreen(doc, element);
  assert.equal(element.requests, 1);
  assert.equal(doc.exits, 1);
  assert.equal(isFullscreen(doc, element), false);
});

test('GIVEN another element in full screen WHEN toggled THEN this element is requested, not exited', async () => {
  const doc = fakeDocument();
  const other = fakeElement(doc);
  await toggleFullscreen(doc, other);
  const element = fakeElement(doc);
  await toggleFullscreen(doc, element);
  assert.equal(element.requests, 1);
  assert.equal(doc.exits, 0);
  assert.equal(isFullscreen(doc, element), true);
  assert.equal(isFullscreen(doc, other), false);
});

test('GIVEN a refused request WHEN toggled THEN nothing is thrown and the document is unchanged', async () => {
  const doc = fakeDocument();
  const element = fakeElement(doc, true);
  await toggleFullscreen(doc, element);
  assert.equal(element.requests, 1);
  assert.equal(doc.fullscreenElement, null);
});

test('GIVEN a failed exit WHEN toggled THEN nothing is thrown', async () => {
  const element = { requestFullscreen: async () => {} };
  const doc = {
    fullscreenElement: element,
    exitFullscreen: async () => {
      throw new TypeError('not full screen');
    }
  };
  await toggleFullscreen(doc, element);
  assert.equal(doc.fullscreenElement, element);
});

const flush = () => new Promise((resolve) => setImmediate(resolve));

test('GIVEN the document enters and leaves full screen WHEN reported THEN the host hears false, true, false', async () => {
  const doc = fakeDocument();
  const element = fakeElement(doc);
  const sent = [];
  const reporter = new FullscreenReporter(doc, async (on) => void sent.push(on));
  reporter.report();
  await flush();
  await toggleFullscreen(doc, element);
  reporter.report();
  await flush();
  await toggleFullscreen(doc, element);
  reporter.report();
  await flush();
  assert.deepEqual(sent, [false, true, false]);
});

test('GIVEN a report that waits WHEN on, off and on follow THEN one send is in flight and the newest state goes last', async () => {
  const doc = { fullscreenElement: null };
  const sent = [];
  const waits = [];
  const reporter = new FullscreenReporter(doc, (on) => {
    sent.push(on);
    return new Promise((resolve) => waits.push(resolve));
  });
  doc.fullscreenElement = {};
  reporter.report();
  doc.fullscreenElement = null;
  reporter.report();
  doc.fullscreenElement = {};
  reporter.report();
  assert.deepEqual(sent, [true]);
  waits[0]();
  await flush();
  assert.deepEqual(sent, [true, true]);
  waits[1]();
  await flush();
  assert.deepEqual(sent, [true, true]);
});

test('GIVEN a report that waits WHEN the state returns to off THEN the last state sent is off', async () => {
  const doc = { fullscreenElement: {} };
  const sent = [];
  const waits = [];
  const reporter = new FullscreenReporter(doc, (on) => {
    sent.push(on);
    return new Promise((resolve) => waits.push(resolve));
  });
  reporter.report();
  doc.fullscreenElement = null;
  reporter.report();
  waits[0]();
  await flush();
  waits[1]();
  await flush();
  assert.deepEqual(sent, [true, false]);
});

test('GIVEN a host that refuses WHEN reported THEN nothing is thrown and the next report still goes', async () => {
  const doc = { fullscreenElement: null };
  const sent = [];
  let fail = true;
  const reporter = new FullscreenReporter(doc, async (on) => {
    sent.push(on);
    if (fail) throw new Error('unavailable');
  });
  assert.doesNotThrow(() => reporter.report());
  await flush();
  fail = false;
  doc.fullscreenElement = {};
  reporter.report();
  await flush();
  assert.deepEqual(sent, [false, true]);
});
