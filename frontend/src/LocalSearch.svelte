<script lang="ts">
  import { onDestroy } from 'svelte';
  import { searchMessages, type LocalSearchHit, type LocalSearchResults } from './bridge';
  import { byteLength } from './drafts';
  import FeatureDialog from './FeatureDialog.svelte';
  import SearchHit from './SearchHit.svelte';

  let { roomId, onselect, onclose }: {
    roomId: string | null; onselect: (hit: LocalSearchHit) => void; onclose: () => void;
  } = $props();
  let term = $state('');
  let scope = $state('all');
  let results = $state<LocalSearchResults | null>(null);
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let request = 0;
  let lastRoom: string | null | undefined;
  $effect(/**
   * Invalidate pending results and errors when the selected room changes.
   */ () => {
    if (roomId === lastRoom) return;
    lastRoom = roomId;
    request += 1;
    results = null;
    busy = false;
    failure = null;
  });
  onDestroy(() => { request += 1; results = null; term = ''; });
  /** Search cached text only; request IDs prevent older results replacing a newer view. */
  async function search(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !term.trim() || byteLength(term) > 128) return;
    const asked = ++request;
    busy = true;
    results = null;
    failure = null;
    try {
      const reply = await searchMessages(term, scope === 'room' ? roomId : null);
      if (asked === request) results = reply;
    } catch {
      if (asked === request) failure = 'The local search failed. Try again after the room cache is available.';
    } finally {
      if (asked === request) busy = false;
    }
  }
</script>

<FeatureDialog title="Search cached messages" {onclose}>
  <p class="note">Search uses only text already in this profile's local cache. Coverage is partial. It does not search the server or fetch missing history.</p>
  <form onsubmit={search}>
    <label for="local-search-term">Text to find (up to 128 UTF-8 bytes)</label>
    <input id="local-search-term" bind:value={term} maxlength="128" autocomplete="off" spellcheck="false" disabled={busy} oninput={() => (results = null)} />
    <label for="local-search-scope">Search in</label>
    <select id="local-search-scope" bind:value={scope} disabled={busy} onchange={() => (results = null)}>
      <option value="all">All cached rooms</option>
      <option value="room" disabled={!roomId}>Current room</option>
    </select>
    <button type="submit" class="primary" disabled={busy || !term.trim() || byteLength(term) > 128}>{busy ? 'Searching…' : 'Search'}</button>
  </form>
  {#if byteLength(term) > 128}<p role="alert" class="error">The search text exceeds 128 UTF-8 bytes.</p>{/if}
  {#if failure}<p role="alert" class="error">{failure}</p>{/if}
  {#if results}
    <p role="status">Scanned {results.scanned} cached events. {results.results.length} results. {results.truncated ? 'The scan or result limit was reached.' : 'Missing and uncached messages are not included.'}</p>
    <ol class="local-results">
      {#each results.results as hit (hit.roomId + hit.eventId)}
        <li><SearchHit {hit} {onselect} /></li>
      {/each}
    </ol>
  {/if}
</FeatureDialog>
