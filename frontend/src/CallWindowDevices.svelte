<script lang="ts">
  import { onMount } from 'svelte';
  import type { CallDeviceChange, CallVideoEffect } from '../../crates/contracts/generated/contracts';
  import { callDevices, callVideoEffect, setCallDevices } from './bridge';
  import { DevicePanel } from './callDevicePanel';
  import CallVideoEffects from './CallVideoEffects.svelte';
  import {
    DEVICES_PANEL_LABEL,
    EFFECT_REFUSED_TEXT,
    NO_VIDEO_EFFECT,
    deviceChoice,
    deviceOptions,
    deviceSwitchText,
    devicesNote,
    videoEffectChange,
    type VideoEffectPart
  } from './calls';

  // The microphone, speaker and camera of the running call (ADR-0065),
  // inside the call window. The list is read each time the panel opens. A
  // choice is saved and applied through the same mid-call switch as the main
  // window's settings (ADR-0059): the core checks the IDs again, and the
  // call status carries a failure as `change`. Device names come from the
  // operating system and show as plain text only. The parent closes the
  // panel (Escape, its button) and moves the focus. `DevicePanel` holds the
  // order rules: saves go one at a time and none is dropped, and a closed
  // panel takes no late answer. The own video effect and the preview's
  // mirror (ADR-0068) go through the same queue, as each save changes the
  // same device record; each sends only the part the user changed, and a
  // failed one puts both controls back to the saved values. `effectRefused`
  // is the call status's: the worker answers after the save's reply, so
  // only the status has its refusal.
  let {
    change = null,
    effectRefused = false
  }: { change?: CallDeviceChange | null; effectRefused?: boolean } = $props();

  let recording = $state('');
  let playout = $state('');
  let camera = $state('');
  let videoEffect = $state<CallVideoEffect>(NO_VIDEO_EFFECT);
  let mirror = $state(true);
  const panel = new DevicePanel(
    { list: callDevices, save: setCallDevices, effect: callVideoEffect },
    (next) => (view = next),
    (menus) => {
      recording = menus.recording;
      playout = menus.playout;
      camera = menus.camera;
      const listed = panel.state.devices;
      if (listed) {
        videoEffect = listed.effect;
        mirror = listed.mirror;
      }
    },
    (next) => (effectView = next),
    (effect, savedMirror) => {
      videoEffect = effect;
      mirror = savedMirror;
    }
  );
  let view = $state(panel.state);
  let effectView = $state(panel.effect);

  onMount(() => {
    void panel.load();
    return () => panel.close();
  });

  // Saves the three menus together; they keep their values on failure. The
  // menus stay usable while a save runs, so the focus stays on them.
  const save = () => void panel.save({ recording, playout, camera });
  const saveEffect = (part: VideoEffectPart) => void panel.saveEffect(videoEffectChange(part, videoEffect, mirror));

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
    <CallVideoEffects
      bind:effect={videoEffect}
      bind:mirror
      available={devices?.backgroundEffects ?? false}
      disabled={!devices || devices.unreadable || view.loading}
      prefix="call-window-effect"
      heading="h3"
      onsave={saveEffect}
    />
    {#if effectRefused}
      <p class="note" role="status">{EFFECT_REFUSED_TEXT}</p>
    {/if}
    {#if effectView.failure}
      <p class="error" role="alert">{effectView.failure}</p>
    {/if}
    <button
      type="button"
      class="secondary"
      disabled={view.loading || view.saving || effectView.saving}
      onclick={() => void panel.load()}
    >
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
