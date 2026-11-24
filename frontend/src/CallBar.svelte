<script lang="ts">
  import type { CallStatus } from '../../crates/contracts/generated/contracts';
  import {
    callAnnouncement,
    CAMERA_TEXT,
    cameraButton,
    DEAFEN_LABEL,
    DEAFEN_TEXT,
    DEVICE_CHANGE_TEXT,
    END_TEXT,
    NO_KEY_TEXT,
    PARTICIPANT_MUTED_LABEL,
    PHASE_TEXT,
    RING_DECLINED_TEXT,
    SCREEN_TEXT,
    screenButton,
    SHARING_TEXT,
    showCallBar,
    SOUND_LEVEL_LABEL,
    SOUND_LEVEL_LOUD,
    SOUND_LEVEL_MAX,
    soundLevelText,
    UNDEAFEN_LABEL,
    UNREACHABLE_TEXT,
    VOLUME_LABEL,
    VOLUME_MAX,
    volumeText,
    volumeValue
  } from './calls';
  import Icon from './Icon.svelte';
  import PersonName from './PersonName.svelte';

  // The call, across every room. Names and room IDs are untrusted text; the
  // core's codes show as fixed texts from calls.ts. The live region stays in
  // the page so that a change in it is announced. Speaking changes are not
  // announced: each participant's mark is read in place only.
  let {
    status,
    dismissed,
    roomName,
    names,
    busy,
    cameraBusy,
    screenBusy,
    volume,
    soundLevel,
    onmute,
    ondeafen,
    onvolume,
    oncamera,
    onscreen,
    onleave,
    ondismiss,
    onwindow = () => {}
  }: {
    status: CallStatus;
    dismissed: boolean;
    roomName: string;
    // Display names from the call room's members, by user ID.
    names: ReadonlyMap<string, string>;
    // A mute, deafen or leave request is waiting for the core.
    busy: boolean;
    // A camera request is waiting for the core.
    cameraBusy: boolean;
    // A screen share request is waiting for the core.
    screenBusy: boolean;
    // The volume the slider shows: the one being set, else the call's.
    volume: number;
    // The loudest call sound now, 0 to SOUND_LEVEL_MAX (ADR-0055).
    soundLevel: number;
    onmute: () => void;
    ondeafen: () => void;
    // The slider moved; the page sends the newest value.
    onvolume: (percent: number) => void;
    oncamera: () => void;
    // Starts or stops the share. The page names no screen: the core asks.
    onscreen: () => void;
    onleave: () => void;
    ondismiss: () => void;
    // Opens the call window, or brings it to the front (ADR-0060).
    onwindow?: () => void;
  } = $props();

  const shown = $derived(showCallBar(status, dismissed));
  const ended = $derived(status.phase === 'ended');
  const camera = $derived(cameraButton(status.camera));
  const cameraText = $derived(CAMERA_TEXT[status.camera]);
  const screen = $derived(screenButton(status.screen));
  const screenText = $derived(SCREEN_TEXT[status.screen]);
</script>

<div class="visually-hidden" aria-live="polite" aria-atomic="true">
  {callAnnouncement(status, dismissed)}
</div>
{#if shown}
  <section class="call-bar" aria-label="Call">
    <div class="call-head">
      <Icon name="phone" />
      <p class="call-room"><bdi>{roomName}</bdi></p>
      <p class="call-phase note">{PHASE_TEXT[status.phase]}</p>
      <span class="call-controls">
        {#if ended}
          <button type="button" class="secondary call-dismiss" onclick={ondismiss}>Dismiss</button>
        {:else}
          <button
            type="button"
            class="secondary call-mute"
            aria-pressed={status.muted}
            disabled={busy}
            onclick={onmute}
          >
            <Icon name={status.muted ? 'mic-off' : 'mic'} />
            {status.muted ? 'Unmute' : 'Mute'}
          </button>
          <button
            type="button"
            class="secondary call-deafen"
            aria-pressed={status.deafened}
            aria-describedby="call-deafen-text"
            disabled={busy}
            onclick={ondeafen}
          >
            <Icon name={status.deafened ? 'headphones-off' : 'headphones'} />
            {status.deafened ? UNDEAFEN_LABEL : DEAFEN_LABEL}
          </button>
          <button
            type="button"
            class="secondary call-camera"
            aria-pressed={camera.pressed}
            disabled={cameraBusy || status.phase !== 'active'}
            onclick={oncamera}
          >
            <Icon name={camera.pressed ? 'video-off' : 'video'} />
            {camera.label}
          </button>
          <button
            type="button"
            class="secondary call-screen"
            aria-pressed={screen.pressed}
            disabled={screenBusy || status.phase !== 'active'}
            onclick={onscreen}
          >
            <Icon name="screen" />
            {screen.label}
          </button>
          <button type="button" class="secondary call-window-open" onclick={onwindow}>
            <Icon name="video" />
            Call window
          </button>
          <button type="button" class="secondary call-leave" onclick={onleave}>Leave</button>
        {/if}
      </span>
    </div>
    {#if !ended && status.ringDeclined}
      <p class="call-declined note" role="status">{RING_DECLINED_TEXT}</p>
    {/if}
    {#if !ended}
      <span id="call-deafen-text" class="visually-hidden">{DEAFEN_TEXT}</span>
      <label class="call-volume">
        <Icon name="volume" />
        <span>{VOLUME_LABEL}</span>
        <input
          type="range"
          min="0"
          max={VOLUME_MAX}
          step="5"
          value={volume}
          aria-valuetext={volumeText(volume)}
          oninput={(event) => onvolume(volumeValue(Number(event.currentTarget.value)))}
        />
        <output class="note">{volumeText(volume)}</output>
      </label>
      <!-- What your microphone picks up: it moves while you talk, so a still
           meter means the call does not hear you. -->
      <div
        class="call-level"
        class:loud={soundLevel >= SOUND_LEVEL_LOUD}
        role="meter"
        aria-label={SOUND_LEVEL_LABEL}
        aria-valuemin="0"
        aria-valuemax={SOUND_LEVEL_MAX}
        aria-valuenow={soundLevel}
        aria-valuetext={soundLevelText(soundLevel)}
      >
        <span class="call-level-fill" style:transform="scaleX({soundLevel / SOUND_LEVEL_MAX})"></span>
      </div>
    {/if}
    {#if !ended && cameraText !== ''}
      <p class="call-camera-note note" role="status">{cameraText}</p>
    {/if}
    {#if !ended && screenText !== ''}
      <p class="call-screen-note note" role="status">{screenText}</p>
    {/if}
    {#if !ended && status.deviceChange}
      <p class="call-device-note note" role="status">{DEVICE_CHANGE_TEXT[status.deviceChange]}</p>
    {/if}
    {#if ended}
      <p class="call-end">{status.ended === null ? END_TEXT.internal : END_TEXT[status.ended]}</p>
    {:else if status.participants.length > 0}
      <ul class="call-people" aria-label="Call participants">
        {#each status.participants as person (person.userId + '/' + person.deviceId)}
          <li>
            {#if person.speaking}
              <span class="speaking-mark" role="img" aria-label="Speaking"></span>
            {:else}
              <span class="speaking-mark idle" aria-hidden="true"></span>
            {/if}
            <PersonName userId={person.userId} name={names.get(person.userId) ?? null} />
            {#if person.muted}<span class="call-muted" role="img" aria-label={PARTICIPANT_MUTED_LABEL}><Icon name="mic-off" /></span>{/if}
            {#if person.sharing}<span class="note call-note call-sharing">{SHARING_TEXT}</span>{/if}
            {#if !person.keyed}<span class="note call-note">{NO_KEY_TEXT}</span>{/if}
            {#if !person.reachable}<span class="note call-note">{UNREACHABLE_TEXT}</span>{/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>
{/if}
