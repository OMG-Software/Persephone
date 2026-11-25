import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  connectionName,
  fps,
  kbps,
  level,
  milliseconds,
  packets,
  parseCallStats,
  percent,
  resolution,
  statsSections,
  UNKNOWN_VALUE
} from '../src/callStats.ts';

const sent = { level: 40, bitrateKbps: 32, packetsSent: 1200, packetsLost: 3, jitterMs: 4, rttMs: 41 };
const video = { fps: 30, width: 1280, height: 720, bitrateKbps: 1500, limit: 'bandwidth', packetsLost: null };
const heardAudio = { level: 55, bitrateKbps: 30, packetsReceived: 5000, packetsLost: 2, jitterMs: 7, concealedPercent: 12 };
const heardVideo = { fps: 24, width: 640, height: 360, bitrateKbps: null, droppedFps: 2, packetsLost: 9 };
const link = { connection: 0, outgoingKbps: 2500, sendKbps: 40, receiveKbps: null, rttMs: 20 };

function reading(more = {}) {
  return {
    microphone: sent,
    camera: video,
    screen: null,
    participants: [{ userId: '@bob:s', deviceId: 'B', audio: heardAudio, video: heardVideo }],
    connections: [link],
    ...more
  };
}

test('GIVEN a number WHEN it is formatted THEN it has its unit, and an unknown value has none', () => {
  assert.equal(kbps(1500), '1,500 kbit/s');
  assert.equal(kbps(0), '0 kbit/s');
  assert.equal(milliseconds(41), '41 ms');
  assert.equal(fps(30), '30 fps');
  assert.equal(percent(12), '12 %');
  assert.equal(packets(1200), '1,200 packets');
  assert.equal(level(40), '40 of 100');
  assert.equal(resolution(1280, 720), '1,280 × 720');
  for (const text of [kbps(null), milliseconds(null), fps(null), percent(null), packets(null)]) {
    assert.equal(text, UNKNOWN_VALUE);
  }
  assert.equal(resolution(0, 0), UNKNOWN_VALUE, 'a track that sends nothing has no size');
  assert.equal(connectionName(0), 'Your call server');
  assert.equal(connectionName(2), 'Other call server 2');
});

test('GIVEN a reading WHEN the sections are made THEN own sources come first, then each device, then each connection', () => {
  const sections = statsSections(reading({ screen: { ...video, limit: 'none' } }));
  assert.deepEqual(
    sections.map((section) => section.title || section.userId),
    ['Your microphone', 'Your camera', 'Your screen share', '@bob:s', 'Your call server']
  );
  const rows = (index) => Object.fromEntries(sections[index].rows.map((row) => [row.label, row.value]));
  assert.deepEqual(rows(0), {
    'Sound level': '40 of 100',
    'Send bitrate': '32 kbit/s',
    'Packets sent': '1,200 packets',
    'Packets lost (far end)': '3 packets',
    Jitter: '4 ms',
    'Round-trip time': '41 ms'
  });
  assert.equal(rows(1)['Limited by'], 'Network bandwidth');
  assert.equal(rows(1)['Packets lost (far end)'], UNKNOWN_VALUE);
  assert.equal(rows(2)['Limited by'], 'None');
  assert.equal(rows(3)['Concealed audio'], '12 %');
  assert.equal(rows(3)['Frames dropped'], '2 fps');
  assert.equal(rows(3)['Video receive bitrate'], UNKNOWN_VALUE);
  assert.equal(rows(4)['Available upload bandwidth'], '2,500 kbit/s');
  assert.equal(rows(4)['Total receive rate'], UNKNOWN_VALUE);
  // A device is shown by its user ID component and its device ID, never as title text.
  assert.equal(sections[3].title, '');
  assert.equal(sections[3].userId, '@bob:s');
  assert.equal(sections[3].deviceId, 'B');
  assert.deepEqual(
    sections.filter((section) => section.userId === null).map((section) => section.deviceId),
    [null, null, null, null]
  );
});

test('GIVEN one person in the call from two devices WHEN the sections are made THEN each section names its own device', () => {
  const sections = statsSections(
    reading({
      microphone: null,
      camera: null,
      participants: [
        { userId: '@bob:s', deviceId: 'PHONE', audio: null, video: null },
        { userId: '@bob:s', deviceId: 'DESK', audio: null, video: null }
      ],
      connections: []
    })
  );
  assert.deepEqual(
    sections.map((section) => [section.userId, section.deviceId]),
    [
      ['@bob:s', 'PHONE'],
      ['@bob:s', 'DESK']
    ]
  );
});

test('GIVEN no camera, no screen and a device without media WHEN the sections are made THEN only what exists shows, and the device says so', () => {
  const sections = statsSections(
    reading({ camera: null, participants: [{ userId: '@eve:s', deviceId: 'E', audio: null, video: null }] })
  );
  assert.deepEqual(
    sections.map((section) => section.key).filter((key) => key === 'camera' || key === 'screen'),
    []
  );
  assert.deepEqual(sections[1].rows, [{ label: 'Receiving', value: 'No audio or video' }]);
});

test('GIVEN a reading inside the contract WHEN it is parsed THEN it is kept', () => {
  assert.deepEqual(parseCallStats(reading()), reading());
  const empty = { microphone: null, camera: null, screen: null, participants: [], connections: [] };
  assert.deepEqual(parseCallStats(empty), empty);
  // Every bound itself passes.
  const top = {
    microphone: { level: 100, bitrateKbps: 1_000_000, packetsSent: 4_294_967_295, packetsLost: null, jitterMs: 60_000, rttMs: 60_000 },
    camera: { fps: 120, width: 16_384, height: 16_384, bitrateKbps: null, limit: 'other', packetsLost: 0 },
    screen: null,
    participants: [],
    connections: [{ connection: 3, outgoingKbps: null, sendKbps: null, receiveKbps: null, rttMs: null }]
  };
  assert.deepEqual(parseCallStats(top), top);
});

test('GIVEN a reading outside the contract WHEN it is parsed THEN it is dropped whole', () => {
  const device = (more = {}) => ({ userId: '@bob:s', deviceId: 'B', audio: null, video: null, ...more });
  const bad = [
    null,
    [],
    'x',
    { ...reading(), extra: 1 },
    { microphone: null },
    reading({ microphone: { ...sent, level: 101 } }),
    reading({ microphone: { ...sent, level: 4.5 } }),
    reading({ microphone: { ...sent, level: -1 } }),
    reading({ microphone: { ...sent, bitrateKbps: 1_000_001 } }),
    reading({ microphone: { ...sent, jitterMs: 60_001 } }),
    reading({ microphone: { ...sent, packetsSent: 4_294_967_296 } }),
    reading({ microphone: { ...sent, rttMs: undefined } }),
    reading({ microphone: { ...sent, rttMs: '41' } }),
    reading({ microphone: { ...sent, address: '10.0.0.1' } }),
    reading({ camera: { ...video, fps: 121 } }),
    reading({ camera: { ...video, width: 16_385 } }),
    reading({ camera: { ...video, limit: 'gremlins' } }),
    reading({ camera: { ...video, codec: 'vp8' } }),
    reading({ screen: { ...video, limit: 5 } }),
    reading({ participants: [device({ audio: { ...heardAudio, concealedPercent: 101 } })] }),
    reading({ participants: [device({ video: { ...heardVideo, droppedFps: 121 } })] }),
    reading({ participants: [device({ userId: 'x'.repeat(256) })] }),
    reading({ participants: [device({ deviceId: 'D'.repeat(256) })] }),
    reading({ participants: [device({ name: 'Bob' })] }),
    reading({ participants: [device(), device()] }),
    reading({ participants: Array.from({ length: 33 }, (_, index) => device({ deviceId: `D${index}` })) }),
    reading({ connections: [{ ...link, connection: 4 }] }),
    reading({ connections: [link, link] }),
    reading({ connections: Array.from({ length: 5 }, (_, index) => ({ ...link, connection: index })) }),
    reading({ connections: [{ ...link, rttMs: 60_001 }] })
  ];
  for (const value of bad) assert.equal(parseCallStats(value), null, JSON.stringify(value));
});

test('GIVEN a hostile user ID WHEN the sections are made THEN it is only text for the page to escape', () => {
  const hostile = '@<img src=x onerror=alert(1)>:s';
  const sections = statsSections(
    reading({ participants: [{ userId: hostile, deviceId: 'B', audio: null, video: null }] })
  );
  const device = sections.find((section) => section.userId !== null);
  assert.equal(device.userId, hostile);
  assert.ok(device.rows.every((row) => !row.value.includes('<')));
});
