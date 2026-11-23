import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { DEFAULT_SETTINGS } from '../src/preferences.ts';
import { OTHER_SWITCHES, deletedParts, itemShown, otherParts, otherSwitch } from '../src/eventText.ts';
import { EVENT_SWITCHES } from '../src/stateText.ts';
import { applyRelations, quoteOf } from '../src/timeline.ts';

const A = '@alice:example.invalid';
const line = (parts) =>
  parts.map((part) => ('person' in part ? part.person : 'name' in part ? `[${part.name}]` : part.text)).join('');

test('GIVEN other events with fixed text WHEN described THEN the line names the sender and never the type', () => {
  const cases = [
    ['verification_request', `${A} sent a verification request`],
    ['verification_ready', `${A} says they are ready to verify`],
    ['verification_done', `${A} says they finished verification`],
    ['call_started', `${A} started a call`],
    ['call_left', `${A} left the call`],
    ['call_updated', `${A} updated their call membership`],
    ['location_stopped', `${A} stopped sharing their location`],
    ['poll_vote', `${A} voted in a poll`],
    ['sticker', `${A} sent a sticker`],
    ['third_party_invite', `${A} invited someone by email or phone`]
  ];
  for (const [event, expected] of cases) {
    assert.equal(line(otherParts(A, event, 'm.secret.type')), expected, event);
  }
});

test('GIVEN other events without fixed text WHEN described THEN the type is its own name piece', () => {
  assert.deepEqual(otherParts(A, 'event', '<b>x</b>'), [
    { person: A },
    { text: ' sent an event of type ' },
    { name: '<b>x</b>' }
  ]);
  assert.equal(line(otherParts(A, 'message', 'org.x')), `${A} sent a message of type [org.x]`);
  assert.equal(line(otherParts(A, 'state', 'org.y')), `${A} changed room state of type [org.y]`);
  // A kind this build does not know is named by its type.
  assert.equal(line(otherParts(A, 'teleport', 'org.z')), `${A} sent an event of type [org.z]`);
  assert.equal(line(otherParts(A, 'toString', 'org.z')), `${A} sent an event of type [org.z]`);
});

test('GIVEN deleted other events WHEN described THEN each names what was deleted, not who deleted it', () => {
  const cases = [
    ['sticker', `${A}: Sticker deleted.`],
    ['poll_started', `${A}: Poll deleted.`],
    ['call_ended', `${A}: Call event deleted.`],
    ['verification_step', `${A}: Verification step deleted.`],
    ['message', `${A}: Message deleted.`],
    ['event', `${A}: Event of type [<b>x</b>] deleted.`],
    // A prototype name is no table entry.
    ['toString', `${A}: Event of type [<b>x</b>] deleted.`]
  ];
  for (const [event, expected] of cases) {
    assert.equal(line(deletedParts(A, event, '<b>x</b>')), expected, event);
  }
});

test('GIVEN every other kind WHEN its switch is read THEN it is the switch its label names', () => {
  const expected = {
    verification_request: 'verification', verification_ready: 'verification', verification_start: 'verification',
    verification_step: 'verification', verification_cancelled: 'verification', verification_done: 'verification',
    call_started: 'calls', call_answered: 'calls', call_declined: 'calls', call_ended: 'calls',
    call_joined: 'calls', call_left: 'calls', call_updated: 'callSignalling', call_signalling: 'callSignalling',
    poll_started: 'polls', poll_vote: 'polls', poll_ended: 'polls', sticker: 'stickers',
    location: 'locations', location_stopped: 'locations', space_child: 'spaces', space_parent: 'spaces',
    widget: 'widgets', third_party_invite: 'invitations', message: null, event: 'otherEvents', state: 'otherEvents'
  };
  for (const [event, key] of Object.entries(expected)) {
    assert.equal(otherSwitch(event), key, event);
  }
  // GIVEN each switch off THEN exactly its kinds go.
  const other = (event) => ({ kind: 'other', event_id: '$o', sender: A, origin_server_ts: null, event, event_type: 't', state: false, deleted: false });
  for (const key of new Set(Object.values(expected).filter(Boolean))) {
    const events = { ...DEFAULT_SETTINGS.timelineEvents, [key]: false };
    for (const [event, owner] of Object.entries(expected)) {
      assert.equal(itemShown(other(event), events), owner !== key, `${event} with ${key} off`);
    }
  }
});

test('GIVEN each other kind WHEN its switch is read THEN it is a listed switch, and an unknown kind is other events', () => {
  const listed = new Set([...EVENT_SWITCHES, ...OTHER_SWITCHES].map((entry) => entry.key));
  for (const event of ['verification_step', 'call_joined', 'call_signalling', 'poll_ended', 'location', 'location_stopped', 'space_parent', 'widget', 'state']) {
    assert.ok(listed.has(otherSwitch(event)), event);
  }
  assert.equal(otherSwitch('third_party_invite'), 'invitations');
  assert.equal(otherSwitch('call_signalling'), 'callSignalling');
  assert.equal(otherSwitch('message'), null);
  assert.equal(otherSwitch('teleport'), 'otherEvents');
  assert.equal(otherSwitch('__proto__'), 'otherEvents');
  // Every switch the settings have is offered once.
  const keys = [...EVENT_SWITCHES, ...OTHER_SWITCHES].map((entry) => entry.key);
  assert.deepEqual([...keys].sort(), Object.keys(DEFAULT_SETTINGS.timelineEvents).sort());
});

test('GIVEN switches off WHEN items are filtered THEN only those kinds go, and encryption notices always show', () => {
  const events = { ...DEFAULT_SETTINGS.timelineEvents, verification: false, deletedMessages: false, joins: false };
  const other = (event, deleted = false) => ({ kind: 'other', event_id: '$o', sender: A, origin_server_ts: null, event, event_type: 't', state: false, deleted });
  const notice = (kind) => ({ kind: 'notice', event_id: '$n', sender: A, notice: kind, origin_server_ts: null });
  assert.equal(itemShown(other('verification_ready'), events), false);
  assert.equal(itemShown(other('call_started'), events), true);
  // GIVEN a deleted other event THEN it shows only while both its kind and
  // deleted messages show.
  const on = DEFAULT_SETTINGS.timelineEvents;
  assert.equal(itemShown(other('poll_started', true), on), true);
  assert.equal(itemShown(other('poll_started', true), { ...on, polls: false }), false);
  // GIVEN other events switched off THEN the kinds with no text of their
  // own, and any kind this build does not know, go; named kinds stay, and
  // a message of an unknown type is still a message.
  const noOther = { ...on, otherEvents: false };
  for (const event of ['event', 'state', 'teleport']) {
    assert.equal(itemShown(other(event), noOther), false, event);
  }
  assert.equal(itemShown(other('sticker'), noOther), true);
  assert.equal(itemShown(other('message'), noOther), true);
  // GIVEN deleted lines switched off THEN a deleted message of an unknown
  // type goes too.
  assert.equal(itemShown(other('message', true), { ...on, deletedMessages: false }), false);
  assert.equal(itemShown(other('poll_started', true), { ...on, deletedMessages: false }), false);
  assert.equal(itemShown(other('poll_started'), { ...on, deletedMessages: false }), true);
  assert.equal(itemShown(notice('deleted'), events), false);
  for (const kind of ['undecryptable', 'withheld', 'untrusted_sender']) {
    assert.equal(itemShown(notice(kind), events), true, kind);
  }
  const join = { kind: 'state', event_id: '$s', sender: A, origin_server_ts: null, change: { type: 'member', target: A, membership: 'joined' } };
  assert.equal(itemShown(join, events), false);
  assert.equal(itemShown(join, DEFAULT_SETTINGS.timelineEvents), true);
  assert.equal(itemShown({ kind: 'message', event_id: '$m', sender: A, body: 'hi', origin_server_ts: null, reply_to: null }, events), true);
});

test('GIVEN deletions of other events WHEN applied THEN a message-like event shows as deleted and a state event keeps its line', () => {
  const other = (id, event, state) => ({ kind: 'other', event_id: id, sender: A, origin_server_ts: 5, event, event_type: 't', state, deleted: false });
  const redaction = (redacts) => ({ kind: 'redaction', event_id: `$r${redacts}`, sender: A, origin_server_ts: 6, redacts });
  // The same kind as a message (m.beacon) and as state (m.beacon_info):
  // the core's state flag decides, not the kind.
  const shown = applyRelations([
    other('$poll', 'poll_started', false),
    other('$space', 'space_child', true),
    other('$beacon', 'location', false),
    other('$beaconInfo', 'location', true),
    redaction('$poll'),
    redaction('$space'),
    redaction('$beacon'),
    redaction('$beaconInfo')
  ]);
  // A message-like other event keeps its kind and is marked deleted; a
  // state one keeps its line as it was.
  assert.deepEqual(
    shown.map((item) => [item.event_id, item.kind, item.event, item.deleted]),
    [
      ['$poll', 'other', 'poll_started', true],
      ['$space', 'other', 'space_child', false],
      ['$beacon', 'other', 'location', true],
      ['$beaconInfo', 'other', 'location', false]
    ]
  );
  assert.deepEqual(quoteOf(shown, '$space'), { sender: A, text: 'An event that is not a message' });
});

/** Cases the core's test (`crates/core/src/timeline_filter.rs`) also reads. */
const SWITCH_CASES = JSON.parse(
  readFileSync(new URL('../../crates/contracts/fixtures/timeline-switches.json', import.meta.url), 'utf8')
);

test('GIVEN the shared switch cases WHEN each hiding switch is off alone THEN the page agrees with the core', () => {
  const keys = Object.keys(DEFAULT_SETTINGS.timelineEvents);
  const switches = (on) => Object.fromEntries(keys.map((key) => [key, on(key)]));
  for (const { item, hiddenBy } of SWITCH_CASES) {
    const name = JSON.stringify(item);
    assert.equal(itemShown(item, DEFAULT_SETTINGS.timelineEvents), true, name);
    for (const key of hiddenBy) {
      assert.ok(keys.includes(key), `${key} is not a switch`);
      assert.equal(itemShown(item, switches((k) => k !== key)), false, `${key}: ${name}`);
    }
    assert.equal(itemShown(item, switches((k) => hiddenBy.includes(k))), true, `another switch hides ${name}`);
  }
  for (const key of keys) {
    assert.ok(SWITCH_CASES.some((c) => c.hiddenBy.includes(key)), `no case for ${key}`);
  }
});
