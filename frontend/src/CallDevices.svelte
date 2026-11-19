<script lang="ts">
  import { onMount } from 'svelte';
  import type { CallDevices } from '../../crates/contracts/generated/contracts';
  import { callDevices, callVolume, setCallDevices } from './bridge';
  import {
    deviceChoice,
    deviceOptions,
    deviceValue,
    devicesNote,
    VOLUME_LABEL,
    VOLUME_MAX,
    VOLUME_SAVE_TEXT,
    volumeText,
    volumeValue
  } from './calls';

  // The call microphone, speaker, camera and volume (ADR-0043, ADR-0048,
  // ADR-0055). The core lists the devices through a
  // contained worker and keeps the choice with the profile. Device names come
  // from the operating system: they are shown as plain text only.
  let devices = $state<CallDevices | null>(null);
  let recording = $state('');
  let playout = $state('');
  let camera = $state('');
  let volume = $state(VOLUME_MAX);
  let volumeSaving = $state(false);
  let loading = $state(false);
  let saving = $state(false);
  let failure = $state<string | null>(null);
  let generation = 0;

  const failures: Record<string, string> = {
    locked: 'The profile is locked.',
    limit_exceeded: 'Persephone is busy. Try again in a moment.',
    core_unavailable: 'The call devices could not be listed.',
    invalid_argument: 'That device cannot be saved.'
  };

  async function load() {
    if (loading) return;
    loading = true;
    failure = null;
    const mine = ++generation;
    try {
      const next = await callDevices();
      if (mine !== generation) return;
      devices = next;
      recording = deviceValue(next.selectedRecording);
      playout = deviceValue(next.selectedPlayout);
      camera = deviceValue(next.selectedCamera);
      volume = next.volume;
    } catch (reason) {
      if (mine === generation) failure = failures[String(reason)] ?? 'The call devices could not be listed.';
    } finally {
      if (mine === generation) loading = false;
    }
  }

  onMount(load);

  // Saves all choices at once; the menus keep the values shown on failure.
  async function save() {
    if (saving) return;
    saving = true;
    failure = null;
    try {
      await setCallDevices(deviceChoice(recording), deviceChoice(playout), deviceChoice(camera));
      if (devices) devices = { ...devices, unreadable: false };
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The call devices could not be saved.';
    } finally {
      saving = false;
    }
  }

  // Saves the volume when the slider is let go. It also applies at once to a
  // call that runs. On failure the slider goes back to the saved value.
  async function saveVolume() {
    if (volumeSaving || !devices) return;
    volumeSaving = true;
    failure = null;
    const wanted = volume;
    try {
      await callVolume(wanted);
      if (devices) devices = { ...devices, volume: wanted };
    } catch (reason) {
      failure = failures[String(reason)] ?? VOLUME_SAVE_TEXT;
      if (devices) volume = devices.volume;
    } finally {
      volumeSaving = false;
    }
  }

  let recordingOptions = $derived(deviceOptions(devices?.recording ?? [], deviceChoice(recording)));
  let playoutOptions = $derived(deviceOptions(devices?.playout ?? [], deviceChoice(playout)));
  let cameraOptions = $derived(deviceOptions(devices?.cameras ?? [], deviceChoice(camera)));
</script>

<div class="call-devices">
  {#if devices && !devices.supported}
    <p class="note">{devicesNote(devices)}</p>
  {:else}
    <label for="call-microphone">Microphone</label>
    <select
      id="call-microphone"
      class="field"
      disabled={!devices || loading || saving}
      bind:value={recording}
      onchange={save}
    >
      {#each recordingOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <label for="call-speaker">Speaker</label>
    <select
      id="call-speaker"
      class="field"
      disabled={!devices || loading || saving}
      bind:value={playout}
      onchange={save}
    >
      {#each playoutOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <label for="call-camera">Camera</label>
    <select
      id="call-camera"
      class="field"
      disabled={!devices || loading || saving}
      bind:value={camera}
      onchange={save}
    >
      {#each cameraOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <label for="call-volume">{VOLUME_LABEL}: <output for="call-volume">{volumeText(volume)}</output></label>
    <input
      id="call-volume"
      type="range"
      min="0"
      max={VOLUME_MAX}
      step="5"
      disabled={!devices || devices.unreadable || loading || volumeSaving}
      value={volume}
      aria-valuetext={volumeText(volume)}
      oninput={(event) => (volume = volumeValue(Number(event.currentTarget.value)))}
      onchange={saveVolume}
    />
    {#if devices}
      <p class="note">{devicesNote(devices)}</p>
    {/if}
    <div class="actions">
      <button type="button" class="secondary" disabled={loading || saving} onclick={load}>
        {loading ? 'Looking for devices…' : 'Refresh devices'}
      </button>
    </div>
  {/if}
  {#if failure}
    <p role="alert" class="error">{failure}</p>
  {/if}
</div>
