<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { connectionHealth } from './bridge';
  import type { ConnectionHealth } from '../../crates/contracts/generated/contracts';
  import { healthLabels } from './healthLabels';

  // One snapshot of the account's connection to its homeserver. The core
  // keeps the numbers; reading them starts no connection and polls nothing.
  // The Connection health dialog and the Diagnostics settings page show it.
  let health = $state<ConnectionHealth | null>(null);
  let busy = $state(false);
  let failure = $state(false);
  let request = 0;
  /** Read one snapshot and discard replies after this panel is destroyed. */
  async function refresh() {
    if (busy) return;
    const asked = ++request;
    busy = true; failure = false;
    try { const reply = await connectionHealth(); if (asked === request) health = reply; }
    catch { if (asked === request) failure = true; }
    finally { if (asked === request) busy = false; }
  }
  onMount(refresh);
  onDestroy(() => { request += 1; health = null; });
  const labels = $derived(health ? healthLabels(health) : null);
  /** Format an observed timestamp in the computer's local time zone. */
  const timestamp = (value: number | null) => value === null ? 'Not recorded' : new Date(value).toLocaleString();
</script>
<p class="note">Snapshot only. Refresh to read current state. This panel does not start a connection or poll the server.</p>
<button type="button" class="secondary" disabled={busy} onclick={refresh}>{busy ? 'Reading…' : 'Refresh'}</button>
{#if failure}<p role="alert" class="error">Connection health could not be read.</p>{/if}
{#if health && labels}
  <dl class="health-list">
    <dt>Account</dt><dd>{labels.account}</dd>
    <dt>Store</dt><dd>{labels.store}</dd>
    <dt>Sync mode</dt><dd>{labels.mode}</dd>
    <dt>Last attempt</dt><dd>{timestamp(health.lastAttemptAt)}</dd>
    <dt>Last success</dt><dd>{timestamp(health.lastSuccessAt)}</dd>
    <dt>Consecutive failures</dt><dd>{health.consecutiveFailures}</dd>
    <dt>Retry delay</dt><dd>{health.retryDelayMs} ms</dd>
    <dt>Last error</dt><dd>{labels.error}</dd>
  </dl>
{/if}
