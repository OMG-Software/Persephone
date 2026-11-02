<script lang="ts">
  import { localZone } from './roomTools';
  let { sender = $bindable(''), from = $bindable(''), before = $bindable(''), disabled = false }:
    { sender?: string; from?: string; before?: string; disabled?: boolean } = $props();
</script>
<fieldset {disabled}>
  <legend>Filters</legend>
  <label>Exact sender Matrix ID <input bind:value={sender} maxlength="255" placeholder="@name:server" autocomplete="off" /></label>
  <label>From (inclusive) <input type="datetime-local" bind:value={from} /></label>
  <label>Before (exclusive) <input type="datetime-local" bind:value={before} /></label>
  <p class="note">Local time zone: {localZone()}. Dates use the original message timestamp.</p>
</fieldset>
