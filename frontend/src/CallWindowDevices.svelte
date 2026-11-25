<script lang="ts">
  import { onMount } from 'svelte';
  import type { CallDeviceChange, CallDevices } from '../../crates/contracts/generated/contracts';
  import { callDevices, setCallDevices } from './bridge';
  import {
    DEVICE_FAILURE_TEXT,
    DEVICES_PANEL_LABEL,
    deviceChoice,
    deviceOptions,
    deviceSwitchText,
    deviceValue,
    devicesNote
  } from './calls';

  // The microphone, speaker and camera of the running call (ADR-0065),
  // inside the call window. The list is read each time the panel opens. A
  // choice is saved and applied through the same mid-call switch as the main
  // window's settings (ADR-0059): the core checks the IDs again, and the
  // call status carries a failure as `change`. Device names come from the
  // operating system and show as plain text only. The parent closes the
  // panel (Escape, its button) and moves the focus.
  let { change = null }: { change?: CallDeviceChange | null } = $props();

  let devices = $state<CallDevices | null>(null);
  let recording = $state('');
  let playout = $state('');
  let camera = $state('');
  let loading = $state(false);
  let saving = $state(false);
  let saved = $state(false);
  let failure = $state<string | null>(null);
  const busy = $derived(loading || saving);
  // A panel that closed does not take a late answer.
  let alive = true;

  async function load() {
    if (busy) return;
    loading = true;
    failure = null;
    try {
      const next = await callDevices();
      if (!alive) return;
      devices = next;
      recording = deviceValue(next.selectedRecording);
      playout = deviceValue(next.selectedPlayout);
      camera = deviceValue(next.selectedCamera);
    } catch (reason) {
      if (alive) failure = DEVICE_FAILURE_TEXT[String(reason)] ?? DEVICE_FAILURE_TEXT.core_unavailable;
    } finally {
      if (alive) loading = false;
    }
  }

  onMount(() => {
    void load();
    return () => (alive = false);
  });

  // Saves the three choices together; the menus keep their values on failure.
  async function save() {
    if (busy) return;
    saving = true;
    saved = false;
    failure = null;
    try {
      await setCallDevices(deviceChoice(recording), deviceChoice(playout), deviceChoice(camera));
      if (alive) saved = true;
    } catch (reason) {
      if (alive) failure = DEVICE_FAILURE_TEXT[String(reason)] ?? 'The call devices could not be saved.';
    } finally {
      if (alive) saving = false;
    }
  }

  const recordingOptions = $derived(deviceOptions(devices?.recording ?? [], deviceChoice(recording)));
  const playoutOptions = $derived(deviceOptions(devices?.playout ?? [], deviceChoice(playout)));
  const cameraOptions = $derived(deviceOptions(devices?.cameras ?? [], deviceChoice(camera)));
  const result = $derived(deviceSwitchText(saved, saving, change));
</script>

<section id="call-window-devices" class="call-window-devices" aria-label={DEVICES_PANEL_LABEL}>
  <h2>{DEVICES_PANEL_LABEL}</h2>
  {#if devices && !devices.supported}
    <p class="note">{devicesNote(devices)}</p>
  {:else}
    <label for="call-window-microphone">Microphone</label>
    <select id="call-window-microphone" class="field" disabled={!devices || busy} bind:value={recording} onchange={save}>
      {#each recordingOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <label for="call-window-speaker">Speaker</label>
    <select id="call-window-speaker" class="field" disabled={!devices || busy} bind:value={playout} onchange={save}>
      {#each playoutOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <label for="call-window-camera">Camera</label>
    <select id="call-window-camera" class="field" disabled={!devices || busy} bind:value={camera} onchange={save}>
      {#each cameraOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <button type="button" class="secondary" disabled={busy} onclick={load}>
      {loading ? 'Looking for devices…' : 'Refresh devices'}
    </button>
  {/if}
  {#if result !== ''}
    <p class="note" role="status">{result}</p>
  {/if}
  {#if failure}
    <p class="error" role="alert">{failure}</p>
  {/if}
</section>
