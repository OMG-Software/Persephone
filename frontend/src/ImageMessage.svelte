<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { TimelineItem } from '../../crates/contracts/generated/contracts';
  import { loadImage, releaseImage, type ImageView } from './bridge';
  import Icon from './Icon.svelte';
  import ImageCanvas from './ImageCanvas.svelte';
  import { canRetryLoad, declaredText, loadErrorText, shownImages } from './image';
  import { avatarColour, initials, isolate } from './people';
  import { clockTime, fullTime, toDate } from './time';

  type ImageItem = Extract<TimelineItem, { kind: 'image' }>;

  // A received image (ADR-0009). Nothing is fetched until the user chooses
  // "Load image". Every field is the sender's: the name is inert, isolated
  // text, and the declared size and dimensions are shown as text only.
  let { roomId, item }: { roomId: string; item: ImageItem } = $props();

  // idle: placeholder. loading: waiting for the core to decode. drawing: the
  // bands are being fetched. shown: drawn, and the core's copy released.
  let phase = $state<'idle' | 'loading' | 'drawing' | 'shown' | 'failed'>('idle');
  let view = $state<ImageView | null>(null);
  let failure = $state<string | null>(null);
  let retry = $state(true);
  // Set when the item leaves the page; a late reply is then released at once.
  let gone = false;
  let attempt = 0;
  let slot: symbol | null = null;

  const date = $derived(toDate(item.origin_server_ts));
  const name = $derived(item.name === '' ? 'Image' : item.name);
  const declared = $derived(declaredText(item.size, item.width, item.height));
  const label = $derived('Image: ' + isolate(name));

  function release(handle: string) {
    // A lock or sign-out already dropped it; nothing else needs doing.
    releaseImage(handle).catch(() => {});
  }

  function fail(reason: unknown) {
    phase = 'failed';
    failure = loadErrorText(reason);
    retry = canRetryLoad(reason);
  }

  async function load() {
    if (phase === 'loading' || phase === 'drawing' || phase === 'shown') return;
    const mine = ++attempt;
    phase = 'loading';
    failure = null;
    try {
      const reply = await loadImage(roomId, item.event_id);
      if (gone || mine !== attempt) {
        release(reply.handle);
        return;
      }
      view = reply;
      phase = 'drawing';
    } catch (reason) {
      if (!gone && mine === attempt) fail(reason);
    }
  }

  function drawn() {
    if (gone || !view) return;
    // The pixels now live in the canvas only.
    release(view.handle);
    phase = 'shown';
    slot = shownImages.admit(() => {
      slot = null;
      view = null;
      phase = 'idle';
    });
  }

  function drawFailed(reason: unknown) {
    if (view) release(view.handle);
    view = null;
    fail(reason);
  }

  onDestroy(() => {
    gone = true;
    if (slot) shownImages.forget(slot);
    if (view && phase === 'drawing') release(view.handle);
  });
</script>

<article class="message" aria-label={'Image from ' + item.sender}>
  <span class="avatar {avatarColour(item.sender)}" aria-hidden="true">{initials(item.sender)}</span>
  <div class="message-body">
    <div class="message-head">
      <strong class="sender"><bdi>{item.sender}</bdi></strong>
      {#if date}
        <time datetime={date.toISOString()} title={fullTime(date)}>{clockTime(date)}</time>
      {/if}
    </div>
    {#if view && (phase === 'drawing' || phase === 'shown')}
      <div class="image-frame" hidden={phase !== 'shown'}>
        {#key view.handle}
          <ImageCanvas {view} {label} ondone={drawn} onerror={drawFailed} />
        {/key}
      </div>
    {/if}
    {#if phase !== 'shown'}
      <div class="attachment">
        <span class="file-tile" aria-hidden="true"><Icon name="image" /></span>
        <div class="attachment-text">
          <p class="attachment-name"><bdi>{name}</bdi></p>
          <p class="attachment-meta">
            {#if declared}{declared} ·{/if}
            {#if item.encrypted}
              <span class="file-enc"><Icon name="lock" />Encrypted</span>
            {:else}
              <span class="file-plain">Not encrypted</span>
            {/if}
          </p>
        </div>
        {#if phase === 'loading' || phase === 'drawing'}
          <p class="note" role="status">Loading image…</p>
        {:else if phase === 'idle' || retry}
          <button type="button" class="secondary" onclick={load}>
            {phase === 'failed' ? 'Try again' : 'Load image'}<span class="visually-hidden">: <bdi>{name}</bdi></span>
          </button>
        {/if}
      </div>
      {#if failure}
        <p class="error" role="alert">{failure}</p>
      {/if}
    {/if}
  </div>
</article>
