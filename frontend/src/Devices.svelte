<script lang="ts">
  import { onMount } from 'svelte';
  import { deviceList, type DeviceList } from './bridge';
  import { fullTime, toDate } from './time';

  // The devices (sessions) signed in to this account, as the homeserver
  // lists them. Their IDs and names are the homeserver's word: isolated text.
  let devices = $state<DeviceList | null>(null);
  let loading = $state(false);
  let failure = $state<string | null>(null);

  const failures: Record<string, string> = {
    core_unavailable: 'The homeserver could not be reached.',
    rejected: 'The homeserver refused to list the sessions.',
    limit_exceeded: 'The sessions are already being listed.',
    locked: 'The session is locked.'
  };

  async function load() {
    if (loading) return;
    loading = true;
    failure = null;
    try {
      devices = await deviceList();
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The sessions could not be listed.';
    } finally {
      loading = false;
    }
  }

  onMount(load);
</script>

<section class="settings-page" aria-labelledby="devices-heading">
  <h2 id="devices-heading">Sessions</h2>
  <p class="lede">
    Every device signed in to your account. A session you do not recognise could read your
    messages: sign it out from a device you trust.
  </p>
  {#if devices}
    <ul class="device-list" aria-label="Signed-in sessions">
      <!-- Keyed by position: two projected IDs from the homeserver can match. -->
      {#each devices.devices as device, index (index)}
        {@const seen = toDate(device.lastSeen)}
        <li class="panel device">
          <p class="device-name">
            <bdi>{device.displayName ?? device.deviceId}</bdi>
            {#if device.current}<span class="badge">This device</span>{/if}
            <span class="badge {device.verified ? 'ok' : 'warn'}">
              {device.verified ? 'Verified' : 'Not verified'}
            </span>
          </p>
          <p class="note">
            ID <bdi class="mono">{device.deviceId}</bdi>
            {#if seen}
              · last seen <time datetime={seen.toISOString()}>{fullTime(seen)}</time>
            {/if}
          </p>
        </li>
      {/each}
    </ul>
    {#if devices.truncated}
      <p class="note">Showing the first {devices.devices.length} sessions.</p>
    {/if}
  {:else if loading}
    <p class="note" role="status">Listing sessions…</p>
  {/if}
  {#if failure}
    <p role="alert" class="error">{failure}</p>
  {/if}
  <div class="actions">
    <button type="button" class="secondary" disabled={loading} onclick={load}>
      {loading ? 'Listing…' : 'Refresh'}
    </button>
  </div>
</section>
