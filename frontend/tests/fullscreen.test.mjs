import assert from 'node:assert/strict';
import { test } from 'node:test';
import { isFullscreen, reportFullscreen, toggleFullscreen } from '../src/fullscreen.ts';

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

test('GIVEN the document enters and leaves full screen WHEN reported THEN the host hears true, then false', async () => {
  const doc = fakeDocument();
  const element = fakeElement(doc);
  const sent = [];
  const send = async (on) => void sent.push(on);
  await reportFullscreen(doc, send);
  await toggleFullscreen(doc, element);
  await reportFullscreen(doc, send);
  await toggleFullscreen(doc, element);
  await reportFullscreen(doc, send);
  assert.deepEqual(sent, [false, true, false]);
});

test('GIVEN a host that refuses WHEN reported THEN nothing is thrown', async () => {
  await reportFullscreen({ fullscreenElement: null }, async () => {
    throw new Error('unavailable');
  });
});
