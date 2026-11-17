<script lang="ts">
  import { onMount } from 'svelte';
  import type { CallDevices } from '../../crates/contracts/generated/contracts';
  import { callDevices, setCallDevices } from './bridge';
  import { deviceChoice, deviceOptions, deviceValue, devicesNote } from './calls';

  // The call microphone and speaker (ADR-0043). The core lists them through a
  // contained worker and keeps the choice with the profile. Device names come
  // from the operating system: they are shown as plain text only.
  let devices = $state<CallDevices | null>(null);
  let recording = $state('');
  let playout = $state('');
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
    } catch (reason) {
      if (mine === generation) failure = failures[String(reason)] ?? 'The call devices could not be listed.';
    } finally {
      if (mine === generation) loading = false;
    }
  }

  onMount(load);

  // Saves both choices at once; the menus keep the values shown on failure.
  async function save() {
    if (saving) return;
    saving = true;
    failure = null;
    try {
      await setCallDevices(deviceChoice(recording), deviceChoice(playout));
      if (devices) devices = { ...devices, unreadable: false };
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The call devices could not be saved.';
    } finally {
      saving = false;
    }
  }

  let recordingOptions = $derived(deviceOptions(devices?.recording ?? [], deviceChoice(recording)));
  let playoutOptions = $derived(deviceOptions(devices?.playout ?? [], deviceChoice(playout)));
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
