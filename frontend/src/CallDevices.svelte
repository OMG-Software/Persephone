<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';
  import type {
    CallDeviceTestKind,
    CallDeviceTestStatus,
    CallDevices
  } from '../../crates/contracts/generated/contracts';
  import {
    callDevices,
    callDeviceTest,
    callDeviceTestStatus,
    callDeviceTestStop,
    callMicrophone,
    callVolume,
    setCallDevices
  } from './bridge';
  import CallVideo from './CallVideo.svelte';
  import {
    DEVICE_TEST_LABEL,
    DEVICE_TEST_STOP_LABEL,
    deviceTestActive,
    deviceTestText,
    IDLE_DEVICE_TEST,
    SOUND_LEVEL_LABEL,
    SOUND_LEVEL_LOUD,
    SOUND_LEVEL_MAX,
    soundLevelText,
    AUTO_LEVEL_HELP,
    AUTO_LEVEL_LABEL,
    MIC_GAIN_DEFAULT,
    MIC_GAIN_HELP,
    MIC_GAIN_LABEL,
    MIC_GAIN_MAX,
    MIC_GAIN_STEP,
    MIC_SAVE_TEXT,
    micGainText,
    micGainValue,
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
  import { DeviceTests, testLevel, testLive, testTiles } from './deviceTests';

  // The call microphone, speaker, camera and volume (ADR-0043, ADR-0048,
  // ADR-0055). The core lists the devices through a
  // contained worker and keeps the choice with the profile. Device names come
  // from the operating system: they are shown as plain text only.
  //
  // Each device has a test (ADR-0059): a contained worker listens on the
  // chosen microphone (the meter moves), plays a tone on the chosen speaker,
  // or shows the chosen camera. `soundLevel` is the own level the core sends,
  // `testTicks` goes up when a test changes and `videoTicks` when a camera
  // picture waits.
  let {
    soundLevel = 0,
    testTicks = 0,
    videoTicks = 0
  }: { soundLevel?: number; testTicks?: number; videoTicks?: number } = $props();

  let test = $state<CallDeviceTestStatus>(IDLE_DEVICE_TEST);
  let testBusy = $state(false);
  // The request rules (deviceTests.ts): newest answer only, and a test that
  // runs or starts late is stopped when the settings close.
  const tests = new DeviceTests(
    { start: callDeviceTest, stop: callDeviceTestStop, status: callDeviceTestStatus },
    (next) => (test = next),
    IDLE_DEVICE_TEST
  );

  $effect(() => {
    testTicks;
    untrack(() => void tests.read());
  });

  // Starts the `kind` test with the device its menu shows, or stops it.
  async function runTest(kind: CallDeviceTestKind) {
    if (testBusy) return;
    testBusy = true;
    try {
      const menu = kind === 'microphone' ? recording : kind === 'speaker' ? playout : camera;
      await tests.run(kind, deviceChoice(menu));
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The test could not start.';
    } finally {
      testBusy = false;
    }
  }

  // Leaving the settings ends a test that runs.
  onDestroy(() => tests.close());

  const testing = $derived(testLive(test));
  const micLevel = $derived(testLevel(test, soundLevel));
  const cameraTiles = $derived(testTiles(test));
  const noNames: ReadonlyMap<string, string> = new Map();

  let devices = $state<CallDevices | null>(null);
  let recording = $state('');
  let playout = $state('');
  let camera = $state('');
  let volume = $state(VOLUME_MAX);
  let volumeSaving = $state(false);
  // The automatic microphone level and the manual gain (ADR-0062).
  let autoLevel = $state(true);
  let micGain = $state(MIC_GAIN_DEFAULT);
  let micSaving = $state(false);
  let loading = $state(false);
  let saving = $state(false);
  // Any request that reads or writes the device record. A refresh or device
  // save during a volume save could bring back the old volume, so each waits.
  const busy = $derived(loading || saving || volumeSaving || micSaving);
  let failure = $state<string | null>(null);
  let generation = 0;

  const failures: Record<string, string> = {
    locked: 'The profile is locked.',
    limit_exceeded: 'Persephone is busy. Try again in a moment.',
    core_unavailable: 'The call devices could not be listed.',
    invalid_argument: 'That device cannot be saved.'
  };

  async function load() {
    if (busy) return;
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
      autoLevel = next.autoLevel;
      micGain = next.micGain;
    } catch (reason) {
      if (mine === generation) failure = failures[String(reason)] ?? 'The call devices could not be listed.';
    } finally {
      if (mine === generation) loading = false;
    }
  }

  onMount(load);

  // Saves all choices at once; the menus keep the values shown on failure.
  async function save() {
    if (busy) return;
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
    if (busy || !devices) return;
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

  // Saves the microphone level and gain, the gain when the slider is let go
  // and the level when the box changes. Both apply at once to a call that
  // runs. On failure they go back to the saved values.
  async function saveMicrophone() {
    if (busy || !devices) return;
    micSaving = true;
    failure = null;
    const wantedLevel = autoLevel;
    const wantedGain = micGain;
    try {
      await callMicrophone(wantedLevel, wantedGain);
      if (devices) devices = { ...devices, autoLevel: wantedLevel, micGain: wantedGain };
    } catch (reason) {
      failure = failures[String(reason)] ?? MIC_SAVE_TEXT;
      if (devices) {
        autoLevel = devices.autoLevel;
        micGain = devices.micGain;
      }
    } finally {
      micSaving = false;
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
      disabled={!devices || busy}
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
      disabled={!devices || busy}
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
      disabled={!devices || busy}
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
      disabled={!devices || devices.unreadable || busy}
      value={volume}
      aria-valuetext={volumeText(volume)}
      oninput={(event) => (volume = volumeValue(Number(event.currentTarget.value)))}
      onchange={saveVolume}
    />
    <label class="switch-row">
      <input
        type="checkbox"
        role="switch"
        disabled={!devices || devices.unreadable || busy}
        bind:checked={autoLevel}
        onchange={saveMicrophone}
      />
      <span>{AUTO_LEVEL_LABEL}</span>
    </label>
    <p class="note switch-note">{AUTO_LEVEL_HELP}</p>
    <label for="call-mic-gain">{MIC_GAIN_LABEL}: <output for="call-mic-gain">{micGainText(micGain)}</output></label>
    <input
      id="call-mic-gain"
      type="range"
      min="0"
      max={MIC_GAIN_MAX}
      step={MIC_GAIN_STEP}
      disabled={!devices || devices.unreadable || busy}
      value={micGain}
      aria-valuetext={micGainText(micGain)}
      oninput={(event) => (micGain = micGainValue(Number(event.currentTarget.value)))}
      onchange={saveMicrophone}
    />
    <p class="note">{MIC_GAIN_HELP}</p>
    {#if devices}
      <p class="note">{devicesNote(devices)}</p>
    {/if}
    <h4 class="device-tests-title">Test your devices</h4>
    <p class="note">Each test uses the device chosen above. It does not join a call.</p>
    {#each ['microphone', 'speaker', 'camera'] as const as kind (kind)}
      {@const active = deviceTestActive(test, kind)}
      {@const text = deviceTestText(test, kind)}
      <div class="device-test">
        <button
          type="button"
          class="secondary"
          aria-pressed={active}
          disabled={!devices || busy || testBusy || (testing && !active)}
          onclick={() => runTest(kind)}
        >
          {active ? DEVICE_TEST_STOP_LABEL : DEVICE_TEST_LABEL[kind]}
        </button>
        {#if kind === 'microphone' && active}
          <div
            class="call-level"
            class:loud={micLevel >= SOUND_LEVEL_LOUD}
            role="meter"
            aria-label={SOUND_LEVEL_LABEL}
            aria-valuemin="0"
            aria-valuemax={SOUND_LEVEL_MAX}
            aria-valuenow={micLevel}
            aria-valuetext={soundLevelText(micLevel)}
          >
            <span class="call-level-fill" style:transform="scaleX({micLevel / SOUND_LEVEL_MAX})"></span>
          </div>
        {/if}
        {#if text !== ''}
          <p class="note" role="status">{text}</p>
        {/if}
      </div>
    {/each}
    {#if cameraTiles.length > 0}
      <CallVideo tiles={cameraTiles} names={noNames} ticks={videoTicks} />
    {/if}
    <div class="actions">
      <button type="button" class="secondary" disabled={busy} onclick={load}>
        {loading ? 'Looking for devices…' : 'Refresh devices'}
      </button>
    </div>
  {/if}
  {#if failure}
    <p role="alert" class="error">{failure}</p>
  {/if}
</div>
