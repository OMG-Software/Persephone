<script lang="ts">
  import {
    RING_TITLE,
    ringButtons,
    ringsAnnouncement,
    type ShownRing
  } from './calls';
  import Icon from './Icon.svelte';
  import PersonName from './PersonName.svelte';

  // Incoming call rings (ADR-0052). Room names and caller IDs are untrusted
  // text, shown escaped and isolated. A ring never joins: only a button does.
  // The prompt shows until the core ends the ring or its time runs out.
  let {
    rings,
    roomNames,
    camera,
    busy,
    onanswer,
    ondecline
  }: {
    rings: ShownRing[];
    // Room display names by room ID; a room without a name shows its ID.
    roomNames: ReadonlyMap<string, string>;
    // A camera is available, so answering with video is offered.
    camera: boolean;
    // Room IDs with an answer or a decline waiting for the core.
    busy: ReadonlySet<string>;
    onanswer: (roomId: string, video: boolean) => void;
    ondecline: (roomId: string) => void;
  } = $props();

  const buttons = $derived(ringButtons(camera));
</script>

<div class="visually-hidden" aria-live="assertive" aria-atomic="true">
  {ringsAnnouncement(rings)}
</div>
{#if rings.length > 0}
  <section class="ring-prompt" aria-label={RING_TITLE}>
    {#each rings as ring (ring.roomId)}
      <div class="ring-row" role="group" aria-label={RING_TITLE}>
        <Icon name="phone" />
        <p class="ring-text">
          <span class="ring-title">{RING_TITLE}</span>
          <span class="ring-caller"><PersonName userId={ring.caller} /></span>
          <span class="note">in <bdi>{roomNames.get(ring.roomId) ?? ring.roomId}</bdi></span>
        </p>
        <span class="call-controls">
          <button
            type="button"
            class="ring-answer"
            disabled={busy.has(ring.roomId)}
            onclick={() => onanswer(ring.roomId, false)}>{buttons.answer}</button
          >
          {#if buttons.video !== null}
            <button
              type="button"
              class="secondary ring-answer-video"
              disabled={busy.has(ring.roomId)}
              onclick={() => onanswer(ring.roomId, true)}>{buttons.video}</button
            >
          {/if}
          <button
            type="button"
            class="secondary ring-decline"
            disabled={busy.has(ring.roomId)}
            onclick={() => ondecline(ring.roomId)}>{buttons.decline}</button
          >
        </span>
      </div>
    {/each}
  </section>
{/if}
