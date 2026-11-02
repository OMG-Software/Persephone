<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';
  import { attachments, pinnedMessages, setPinned } from './bridge';
  import type { AttachmentKind, PinnedMessages, TimelineItem } from '../../crates/contracts/generated/contracts';
  import PinnedMessageRow from './PinnedMessageRow.svelte';
  import FeatureDialog from './FeatureDialog.svelte';
  import SearchFilters from './SearchFilters.svelte';
  import FileMessage from './FileMessage.svelte';
  import ImageMessage from './ImageMessage.svelte';
  import { appendAttachments, createAttachmentQueryGuard, localTimestamp, localZone, searchFilters } from './roomTools';
  let { roomId, mode, onclose, ondate, onevent }: {
    roomId: string; mode: 'jump' | 'pins' | 'attachments'; onclose: () => void;
    ondate: (timestamp: number) => Promise<void>; onevent: (eventId: string) => Promise<void>;
  } = $props();
  let date = $state('');
  let sender = $state(''); let from = $state(''); let before = $state('');
  let kind = $state<AttachmentKind>('all');
  let pins = $state<PinnedMessages | null>(null);
  let items = $state<TimelineItem[]>([]);
  let cursor = $state<string | null>(null);
  let scanned = $state(0); let partial = $state(false);
  let busy = $state(false); let failure = $state<string | null>(null);
  let request = 0;
  let appliedQuery = $state<string | null>(null);
  const query = $derived(JSON.stringify([kind, sender, from, before]));
  const filters = $derived(searchFilters(sender, from, before));
  const title = $derived(mode === 'jump' ? 'Jump to date' : mode === 'pins' ? 'Pinned messages' : 'Attachments');
  const queryGuard = createAttachmentQueryGuard();
  /** Clear content and invalidate requests once per attachment filter change. */
  function changedQuery() {
    const next = query;
    if (mode !== 'attachments') return;
    untrack(() => {
      if (!queryGuard.select(next)) return;
      request += 1;
      items = []; cursor = null; scanned = 0; partial = false; appliedQuery = null;
      busy = false; failure = null;
    });
  }
  $effect(changedQuery);
  /** Fetch one bounded page; only explicit Load more appends matching metadata. */
  async function load(more = false) {
    if (busy || (mode === 'attachments' && !filters)) return;
    const asked = ++request;
    const askedQuery = query;
    const queryCurrent = queryGuard.capture(askedQuery);
    const valid = () => asked === request && (mode !== 'attachments' || (queryCurrent() && askedQuery === query));
    busy = true; failure = null;
    if (mode === 'attachments' && !more) { items = []; cursor = null; scanned = 0; partial = false; appliedQuery = null; }
    try {
      if (mode === 'pins') {
        const reply = await pinnedMessages(roomId);
        if (valid() && reply.roomId === roomId) pins = reply;
      } else if (mode === 'attachments') {
        const reply = await attachments(roomId, more ? cursor : null, kind, filters!);
        if (valid() && reply.roomId === roomId) {
          items = appendAttachments(more ? items : [], reply.items);
          appliedQuery = askedQuery; cursor = reply.nextCursor; scanned = more ? scanned + reply.scanned : reply.scanned; partial = reply.partial;
        }
      }
    } catch { if (valid()) failure = 'The room data could not be loaded. Try again.'; }
    finally { if (valid()) busy = false; }
  }
  /** Request a core-authorized pin removal and ignore replies after destruction. */
  async function unpin(eventId: string) {
    if (busy || !pins?.canManage) return;
    const asked = ++request; busy = true; failure = null;
    try { await setPinned(roomId, eventId, false); if (asked === request && pins) pins = { ...pins, eventIds: pins.eventIds.filter((id) => id !== eventId) }; }
    catch { if (asked === request) failure = 'The pin could not be removed. Room permission may have changed.'; }
    finally { if (asked === request) busy = false; }
  }
  /** Keep navigation errors in this dialog while the owner guards timeline state. */
  async function navigate(action: () => Promise<void>) {
    if (busy) return;
    const asked = ++request; busy = true; failure = null;
    try { await action(); }
    catch { if (asked === request) failure = 'The message context could not be loaded.'; }
    finally { if (asked === request) busy = false; }
  }
  onMount(() => { if (mode === 'pins') void load(); });
  onDestroy(() => { request += 1; items = []; pins = null; });
</script>
<FeatureDialog {title} {onclose}>
  {#if mode === 'jump'}
    <form onsubmit={(event) => { event.preventDefault(); const timestamp = localTimestamp(date); if (timestamp !== null) void navigate(() => ondate(timestamp)); }}>
      <label>Local date and time <input type="datetime-local" bind:value={date} disabled={busy} required /></label>
      <p class="note">Time zone: {localZone()}. Find a message near the chosen time.</p>
      <button type="submit" class="primary" disabled={busy || localTimestamp(date) === null}>{busy ? 'Finding…' : 'Jump'}</button>
    </form>
  {:else if mode === 'pins'}
    <p class="note">Pins are shared room state. Other room members can see the pinned message references.</p>
    <button type="button" class="secondary" disabled={busy} onclick={() => load()}>Refresh pins</button>
    {#if pins}
      <p class="note">{pins.canManage ? 'You can manage pins.' : 'You do not have permission to manage pins.'} {pins.truncated ? 'Some previews are unavailable or a loading limit was reached.' : ''}</p>
      {#if pins.eventIds.length === 0}<p>No pinned messages.</p>{/if}
      <ol class="local-results">
        {#each pins.eventIds.slice(0, 100) as eventId (eventId)}
          {@const item = pins.items.find((entry) => entry.kind !== 'gap' && entry.event_id === eventId)}
          <li>
            <PinnedMessageRow {item} {eventId} canManage={pins.canManage} {busy}
              onview={() => navigate(() => onevent(eventId))} onunpin={() => unpin(eventId)} />
          </li>
        {/each}
      </ol>
    {/if}
  {:else}
    <p class="note">Browse bounded server history pages. Results can be partial. Media loads only when you choose Load image or Save.</p>
    <form onsubmit={(event) => { event.preventDefault(); void load(); }}>
      <label>Attachment kind <select bind:value={kind} disabled={busy}><option value="all">All attachments</option><option value="images">Images</option><option value="files">Files</option></select></label>
      <SearchFilters bind:sender bind:from bind:before disabled={busy} />
      {#if !filters}<p role="alert" class="error">Enter a valid Matrix ID and date range. From must be before Before.</p>{/if}
      <button type="submit" class="primary" disabled={busy || !filters}>Browse</button>
    </form>
    <p role="status">Scanned {scanned} events. Showing {items.length} attachments. {partial ? 'History coverage is partial.' : ''}</p>
    <ol class="local-results">
      {#each items as item (item.kind === 'gap' ? '' : item.event_id)}
        <li>{#if item.kind === 'image'}<ImageMessage {roomId} {item} />{:else if item.kind === 'file'}<FileMessage {roomId} {item} />{/if}</li>
      {/each}
    </ol>
    <button type="button" class="secondary" disabled={busy || (!cursor || appliedQuery !== query) || items.length >= 500 || !filters} onclick={() => load(true)}>Load more</button>
    {#if items.length >= 500}<p class="note">The 500 attachment display limit was reached. Narrow the filters.</p>{/if}
  {/if}
  {#if busy}<p role="status">Loading…</p>{/if}
  {#if failure}<p role="alert" class="error">{failure}</p>{/if}
</FeatureDialog>
