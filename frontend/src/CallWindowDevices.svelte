<script lang="ts">
  import { onMount } from 'svelte';
  import type { CallDeviceChange } from '../../crates/contracts/generated/contracts';
  import { callDevices, setCallDevices } from './bridge';
  import { DevicePanel } from './callDevicePanel';
  import { DEVICES_PANEL_LABEL, deviceChoice, deviceOptions, deviceSwitchText, devicesNote } from './calls';

  // The microphone, speaker and camera of the running call (ADR-0065),
  // inside the call window. The list is read each time the panel opens. A
  // choice is saved and applied through the same mid-call switch as the main
  // window's settings (ADR-0059): the core checks the IDs again, and the
  // call status carries a failure as `change`. Device names come from the
  // operating system and show as plain text only. The parent closes the
  // panel (Escape, its button) and moves the focus. `DevicePanel` holds the
  // order rules: saves go one at a time and none is dropped, and a closed
  // panel takes no late answer.
  let { change = null }: { change?: CallDeviceChange | null } = $props();

  let recording = $state('');
  let playout = $state('');
  let camera = $state('');
  const panel = new DevicePanel(
    { list: callDevices, save: setCallDevices },
    (next) => (view = next),
    (menus) => {
      recording = menus.recording;
      playout = menus.playout;
      camera = menus.camera;
    }
  );
  let view = $state(panel.state);

  onMount(() => {
    void panel.load();
    return () => panel.close();
  });

  // Saves the three menus together; they keep their values on failure. The
  // menus stay usable while a save runs, so the focus stays on them.
  const save = () => void panel.save({ recording, playout, camera });

  const devices = $derived(view.devices);
  const recordingOptions = $derived(deviceOptions(devices?.recording ?? [], deviceChoice(recording)));
  const playoutOptions = $derived(deviceOptions(devices?.playout ?? [], deviceChoice(playout)));
  const cameraOptions = $derived(deviceOptions(devices?.cameras ?? [], deviceChoice(camera)));
  const result = $derived(deviceSwitchText(view.saved, view.saving, change));
</script>

<section id="call-window-devices" class="call-window-devices" aria-label={DEVICES_PANEL_LABEL}>
  <h2>{DEVICES_PANEL_LABEL}</h2>
  {#if devices && !devices.supported}
    <p class="note">{devicesNote(devices)}</p>
  {:else}
    <label for="call-window-microphone">Microphone</label>
    <select id="call-window-microphone" class="field" disabled={!devices || view.loading} bind:value={recording} onchange={save}>
      {#each recordingOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <label for="call-window-speaker">Speaker</label>
    <select id="call-window-speaker" class="field" disabled={!devices || view.loading} bind:value={playout} onchange={save}>
      {#each playoutOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <label for="call-window-camera">Camera</label>
    <select id="call-window-camera" class="field" disabled={!devices || view.loading} bind:value={camera} onchange={save}>
      {#each cameraOptions as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
    <button type="button" class="secondary" disabled={view.loading || view.saving} onclick={() => void panel.load()}>
      {view.loading ? 'Looking for devices…' : 'Refresh devices'}
    </button>
  {/if}
  {#if result !== ''}
    <p class="note" role="status">{result}</p>
  {/if}
  {#if view.failure}
    <p class="error" role="alert">{view.failure}</p>
  {/if}
</section>
