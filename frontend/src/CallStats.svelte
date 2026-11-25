<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import type { CallStats } from '../../crates/contracts/generated/contracts';
  import { startCallStats, stopCallStats } from './bridge';
  import { STATS_FAILED_TEXT, STATS_LABEL, STATS_WAITING_TEXT, statsSections } from './callStats';
  import Icon from './Icon.svelte';
  import PersonName from './PersonName.svelte';

  // The call's technical details (ADR-0066), shown while this panel exists:
  // mounting it turns the readings on, and destroying it turns them off, so a
  // closed panel, a closed window and an ended call all stop them. Every
  // number comes from the host's reading and shows with its unit. User IDs
  // are untrusted text.
  let { onclose }: { onclose: () => void } = $props();
  let stats = $state<CallStats | null>(null);
  let failed = $state(false);
  let closeButton = $state<HTMLButtonElement>();
  // The stop waits for the start, so a quick close never leaves them on.
  let starting: Promise<void> = Promise.resolve();

  onMount(() => {
    closeButton?.focus();
    starting = startCallStats((next) => (stats = next)).catch(() => {
      failed = true;
    });
  });

  onDestroy(() => {
    void starting.then(() => stopCallStats()).catch(() => {});
  });

  const sections = $derived(stats ? statsSections(stats) : []);
</script>

<svelte:window
  onkeydown={(event) => {
    if (event.key === 'Escape') onclose();
  }}
/>

<div class="call-stats" role="dialog" aria-label={STATS_LABEL}>
  <header class="call-stats-head">
    <h2>{STATS_LABEL}</h2>
    <button type="button" class="secondary" aria-label="Close {STATS_LABEL.toLowerCase()}" bind:this={closeButton} onclick={onclose}>
      <Icon name="close" />
    </button>
  </header>
  {#if failed}
    <p class="error" role="alert">{STATS_FAILED_TEXT}</p>
  {:else if stats === null}
    <p class="note">{STATS_WAITING_TEXT}</p>
  {:else}
    {#each sections as section (section.key)}
      <table class="call-stats-table">
        <caption>
          {#if section.userId !== null}<PersonName userId={section.userId} />{:else}{section.title}{/if}
        </caption>
        <tbody>
          {#each section.rows as row (row.label)}
            <tr><th scope="row">{row.label}</th><td>{row.value}</td></tr>
          {/each}
        </tbody>
      </table>
    {/each}
  {/if}
</div>
