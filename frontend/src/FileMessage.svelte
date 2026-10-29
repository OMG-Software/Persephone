<script lang="ts">
  import type { TimelineItem } from '../../crates/contracts/generated/contracts';
  import { saveFile } from './bridge';
  import { avatarColour, initials } from './people';
  import { clockTime, fullTime, toDate } from './time';

  // A received file (ADR-0029). Nothing is fetched, opened or played: the
  // core downloads and decrypts it only when the user saves it, and asks
  // where in its own Save dialog. The name is the sender's word, made safe
  // by the core: isolated text.
  let {
    roomId,
    item
  }: {
    roomId: string;
    item: Extract<TimelineItem, { kind: 'file' }>;
  } = $props();

  const date = $derived(toDate(item.origin_server_ts));
  let saving = $state(false);
  let status = $state<string | null>(null);

  const failures: Record<string, string> = {
    rejected: 'The file is larger than 20 MB, or it failed its integrity check.',
    not_found: 'The file is no longer available.',
    core_unavailable: 'The file could not be downloaded or saved.',
    limit_exceeded: 'Another file or image is loading. Try again in a moment.',
    locked: 'The session is locked.'
  };

  function sizeText(bytes: number | null): string {
    if (bytes === null) return 'size not given';
    if (bytes < 1024) return `${bytes} bytes`;
    if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  async function save() {
    if (saving) return;
    saving = true;
    status = null;
    try {
      status = (await saveFile(roomId, item.event_id)) ? 'Saved.' : null;
    } catch (reason) {
      status = failures[String(reason)] ?? 'The file could not be saved.';
    } finally {
      saving = false;
    }
  }
</script>

<article class="message" aria-label={'File from ' + item.sender}>
  <span class="avatar {avatarColour(item.sender)}" aria-hidden="true">{initials(item.sender)}</span>
  <div class="message-body">
    <div class="message-head">
      <strong class="sender"><bdi>{item.sender}</bdi></strong>
      {#if date}
        <time datetime={date.toISOString()} title={fullTime(date)}>{clockTime(date)}</time>
      {/if}
    </div>
    <div class="file-card">
      <span class="file-tile" aria-hidden="true">FILE</span>
      <div class="file-text">
        <bdi class="file-name">{item.name}</bdi>
        <span class="note"
          >{sizeText(item.size)}{item.encrypted ? ' · encrypted' : ' · not encrypted'}</span
        >
      </div>
      <button type="button" class="secondary" disabled={saving} onclick={save}>
        {saving ? 'Saving…' : 'Save…'}
        <span class="visually-hidden"><bdi>{item.name}</bdi></span>
      </button>
    </div>
    {#if status}
      <p class="note" role="status">{status}</p>
    {/if}
  </div>
</article>
