// The call's technical details (ADR-0066): reading what the host sends, and
// the text of each number with its unit. Pure: no host access and no DOM, so
// Node tests load it directly.

import type {
  CallStats,
  CallStatsAudioReceived,
  CallStatsAudioSent,
  CallStatsConnection,
  CallStatsLimit,
  CallStatsParticipant,
  CallStatsVideoReceived,
  CallStatsVideoSent
} from '../../crates/contracts/generated/contracts';

// The bounds of the Rust contract (`crates/contracts`). The host drops a
// reading outside them; this page checks again before it shows anything.
const KBPS_MAX = 1_000_000;
const MS_MAX = 60_000;
const FPS_MAX = 120;
const SIDE_MAX = 16_384;
const LEVEL_MAX = 100;
const PARTICIPANTS_MAX = 32;
const CONNECTIONS_MAX = 4;
const USER_ID_BYTES_MAX = 255;
const DEVICE_ID_BYTES_MAX = 255;
const U32_MAX = 4_294_967_295;

type Fields = Record<string, unknown>;

function isFields(value: unknown): value is Fields {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Whether `value` has exactly the keys `names`: a missing or extra one is a different shape. */
function hasKeys(value: Fields, names: readonly string[]): boolean {
  const keys = Object.keys(value);
  return keys.length === names.length && names.every((name) => Object.hasOwn(value, name));
}

function whole(value: unknown, max: number): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= max;
}

function optional(value: unknown, max: number): value is number | null {
  return value === null || whole(value, max);
}

function utf8Bytes(text: string): number {
  return new TextEncoder().encode(text).length;
}

const LIMITS: readonly string[] = ['none', 'cpu', 'bandwidth', 'other'];

function audioSent(value: unknown): value is CallStatsAudioSent {
  return (
    isFields(value) &&
    hasKeys(value, ['level', 'bitrateKbps', 'packetsSent', 'packetsLost', 'jitterMs', 'rttMs']) &&
    whole(value.level, LEVEL_MAX) &&
    optional(value.bitrateKbps, KBPS_MAX) &&
    whole(value.packetsSent, U32_MAX) &&
    optional(value.packetsLost, U32_MAX) &&
    optional(value.jitterMs, MS_MAX) &&
    optional(value.rttMs, MS_MAX)
  );
}

function videoSent(value: unknown): value is CallStatsVideoSent {
  return (
    isFields(value) &&
    hasKeys(value, ['fps', 'width', 'height', 'bitrateKbps', 'limit', 'packetsLost']) &&
    whole(value.fps, FPS_MAX) &&
    whole(value.width, SIDE_MAX) &&
    whole(value.height, SIDE_MAX) &&
    optional(value.bitrateKbps, KBPS_MAX) &&
    typeof value.limit === 'string' &&
    LIMITS.includes(value.limit) &&
    optional(value.packetsLost, U32_MAX)
  );
}

function audioReceived(value: unknown): value is CallStatsAudioReceived {
  return (
    isFields(value) &&
    hasKeys(value, ['level', 'bitrateKbps', 'packetsReceived', 'packetsLost', 'jitterMs', 'concealedPercent']) &&
    whole(value.level, LEVEL_MAX) &&
    optional(value.bitrateKbps, KBPS_MAX) &&
    whole(value.packetsReceived, U32_MAX) &&
    whole(value.packetsLost, U32_MAX) &&
    whole(value.jitterMs, MS_MAX) &&
    optional(value.concealedPercent, 100)
  );
}

function videoReceived(value: unknown): value is CallStatsVideoReceived {
  return (
    isFields(value) &&
    hasKeys(value, ['fps', 'width', 'height', 'bitrateKbps', 'droppedFps', 'packetsLost']) &&
    whole(value.fps, FPS_MAX) &&
    whole(value.width, SIDE_MAX) &&
    whole(value.height, SIDE_MAX) &&
    optional(value.bitrateKbps, KBPS_MAX) &&
    optional(value.droppedFps, FPS_MAX) &&
    whole(value.packetsLost, U32_MAX)
  );
}

function orNull<T>(value: unknown, check: (value: unknown) => value is T): boolean {
  return value === null || check(value);
}

function participant(value: unknown): value is CallStatsParticipant {
  return (
    isFields(value) &&
    hasKeys(value, ['userId', 'deviceId', 'audio', 'video']) &&
    typeof value.userId === 'string' &&
    typeof value.deviceId === 'string' &&
    utf8Bytes(value.userId) <= USER_ID_BYTES_MAX &&
    utf8Bytes(value.deviceId) <= DEVICE_ID_BYTES_MAX &&
    orNull(value.audio, audioReceived) &&
    orNull(value.video, videoReceived)
  );
}

function connection(value: unknown): value is CallStatsConnection {
  return (
    isFields(value) &&
    hasKeys(value, ['connection', 'outgoingKbps', 'sendKbps', 'receiveKbps', 'rttMs']) &&
    whole(value.connection, CONNECTIONS_MAX - 1) &&
    optional(value.outgoingKbps, KBPS_MAX) &&
    optional(value.sendKbps, KBPS_MAX) &&
    optional(value.receiveKbps, KBPS_MAX) &&
    optional(value.rttMs, MS_MAX)
  );
}

/**
 * The reading the host sent, or `null` when the value is anything but the
 * contract: the exact fields, every number a whole number inside its bound,
 * at most 32 devices and 4 connections, each device and connection once.
 * A bad reading is dropped whole.
 */
export function parseCallStats(value: unknown): CallStats | null {
  if (!isFields(value) || !hasKeys(value, ['microphone', 'camera', 'screen', 'participants', 'connections'])) {
    return null;
  }
  const { microphone, camera, screen, participants, connections } = value;
  if (
    !orNull(microphone, audioSent) ||
    !orNull(camera, videoSent) ||
    !orNull(screen, videoSent) ||
    !Array.isArray(participants) ||
    participants.length > PARTICIPANTS_MAX ||
    !participants.every(participant) ||
    !Array.isArray(connections) ||
    connections.length > CONNECTIONS_MAX ||
    !connections.every(connection)
  ) {
    return null;
  }
  const devices = new Set(participants.map((entry) => JSON.stringify([entry.userId, entry.deviceId])));
  const links = new Set(connections.map((entry) => entry.connection));
  if (devices.size !== participants.length || links.size !== connections.length) return null;
  return value as unknown as CallStats;
}

/** Shown for a value that is not known yet. */
export const UNKNOWN_VALUE = '—';

function number(value: number): string {
  return value.toLocaleString('en-US');
}

/** A bitrate, in kbit/s. */
export function kbps(value: number | null): string {
  return value === null ? UNKNOWN_VALUE : `${number(value)} kbit/s`;
}

/** A time, in milliseconds. */
export function milliseconds(value: number | null): string {
  return value === null ? UNKNOWN_VALUE : `${number(value)} ms`;
}

/** A frame rate. */
export function fps(value: number | null): string {
  return value === null ? UNKNOWN_VALUE : `${number(value)} fps`;
}

/** A share, in percent. */
export function percent(value: number | null): string {
  return value === null ? UNKNOWN_VALUE : `${number(value)} %`;
}

/** A count of packets. */
export function packets(value: number | null): string {
  return value === null ? UNKNOWN_VALUE : `${number(value)} packets`;
}

/** A sound level, 0 to 100 on the meter's scale. */
export function level(value: number): string {
  return `${number(value)} of ${LEVEL_MAX}`;
}

/** A picture size. A track that sends nothing yet has none. */
export function resolution(width: number, height: number): string {
  return width === 0 || height === 0 ? UNKNOWN_VALUE : `${number(width)} × ${number(height)}`;
}

/** Why a sent video runs below what was asked. */
export const LIMIT_TEXT: Record<CallStatsLimit, string> = {
  none: 'None',
  cpu: 'Processor',
  bandwidth: 'Network bandwidth',
  other: 'Other'
};

/** The name of a connection to a call server (ADR-0054). */
export function connectionName(index: number): string {
  return index === 0 ? 'Your call server' : `Other call server ${index}`;
}

/** One line of a table: what it measures and its value with its unit. */
export interface StatsRow {
  label: string;
  value: string;
}

/** The numbers of one source. A device shows its user ID through `userId`; the other sources have a title. */
export interface StatsSection {
  key: string;
  title: string;
  userId: string | null;
  rows: StatsRow[];
}

function audioSentRows(audio: CallStatsAudioSent): StatsRow[] {
  return [
    { label: 'Sound level', value: level(audio.level) },
    { label: 'Send bitrate', value: kbps(audio.bitrateKbps) },
    { label: 'Packets sent', value: packets(audio.packetsSent) },
    { label: 'Packets lost (far end)', value: packets(audio.packetsLost) },
    { label: 'Jitter', value: milliseconds(audio.jitterMs) },
    { label: 'Round-trip time', value: milliseconds(audio.rttMs) }
  ];
}

function videoSentRows(video: CallStatsVideoSent): StatsRow[] {
  return [
    { label: 'Frame rate', value: fps(video.fps) },
    { label: 'Resolution', value: resolution(video.width, video.height) },
    { label: 'Send bitrate', value: kbps(video.bitrateKbps) },
    { label: 'Limited by', value: LIMIT_TEXT[video.limit] },
    { label: 'Packets lost (far end)', value: packets(video.packetsLost) }
  ];
}

function audioReceivedRows(audio: CallStatsAudioReceived): StatsRow[] {
  return [
    { label: 'Audio level', value: level(audio.level) },
    { label: 'Receive bitrate', value: kbps(audio.bitrateKbps) },
    { label: 'Packets received', value: packets(audio.packetsReceived) },
    { label: 'Packets lost', value: packets(audio.packetsLost) },
    { label: 'Jitter', value: milliseconds(audio.jitterMs) },
    { label: 'Concealed audio', value: percent(audio.concealedPercent) }
  ];
}

function videoReceivedRows(video: CallStatsVideoReceived): StatsRow[] {
  return [
    { label: 'Video frame rate', value: fps(video.fps) },
    { label: 'Video resolution', value: resolution(video.width, video.height) },
    { label: 'Video receive bitrate', value: kbps(video.bitrateKbps) },
    { label: 'Frames dropped', value: fps(video.droppedFps) },
    { label: 'Video packets lost', value: packets(video.packetsLost) }
  ];
}

function receivedRows(entry: CallStatsParticipant): StatsRow[] {
  const rows = [...(entry.audio ? audioReceivedRows(entry.audio) : []), ...(entry.video ? videoReceivedRows(entry.video) : [])];
  return rows.length > 0 ? rows : [{ label: 'Receiving', value: 'No audio or video' }];
}

/**
 * The reading as sections of rows, in the order a person looks for them: the
 * own microphone, camera and screen share (when on), each other device, then
 * the connections. A device that sends neither audio nor video has one row that
 * says so.
 */
export function statsSections(stats: CallStats): StatsSection[] {
  const sections: StatsSection[] = [];
  if (stats.microphone) {
    sections.push({ key: 'microphone', title: 'Your microphone', userId: null, rows: audioSentRows(stats.microphone) });
  }
  if (stats.camera) {
    sections.push({ key: 'camera', title: 'Your camera', userId: null, rows: videoSentRows(stats.camera) });
  }
  if (stats.screen) {
    sections.push({ key: 'screen', title: 'Your screen share', userId: null, rows: videoSentRows(stats.screen) });
  }
  for (const entry of stats.participants) {
    sections.push({
      key: JSON.stringify(['device', entry.userId, entry.deviceId]),
      title: '',
      userId: entry.userId,
      rows: receivedRows(entry)
    });
  }
  for (const link of stats.connections) {
    sections.push({
      key: `connection ${link.connection}`,
      title: connectionName(link.connection),
      userId: null,
      rows: [
        { label: 'Available upload bandwidth', value: kbps(link.outgoingKbps) },
        { label: 'Total send rate', value: kbps(link.sendKbps) },
        { label: 'Total receive rate', value: kbps(link.receiveKbps) },
        { label: 'Round-trip time', value: milliseconds(link.rttMs) }
      ]
    });
  }
  return sections;
}

/** Said while no reading has arrived. */
export const STATS_WAITING_TEXT = 'Waiting for the first reading.';
/** Said when the details cannot be turned on. */
export const STATS_FAILED_TEXT = 'The technical details could not be started.';
/** The button that opens the panel, and the panel's name. */
export const STATS_LABEL = 'Technical details';
