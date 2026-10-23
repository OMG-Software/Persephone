<script lang="ts">
  import { onMount } from 'svelte';
  import type { RoomSummary } from '../../crates/contracts/generated/contracts';
  import EncryptionIcon from './EncryptionIcon.svelte';
  import Icon from './Icon.svelte';

  // Jumps to a room by name. It filters the room list already loaded; it
  // searches no messages and sends nothing to the core.
  let {
    rooms,
    onselect,
    onclose
  }: {
    rooms: RoomSummary[];
    onselect: (roomId: string) => void;
    onclose: () => void;
  } = $props();

  // Results shown at most; the list is already bounded by the core.
  const MAX_RESULTS = 50;

  let query = $state('');
  let active = $state(0);
  let dialog = $state<HTMLDialogElement | null>(null);

  const name = (room: RoomSummary) => room.displayName ?? room.roomId;

  const results = $derived.by(() => {
    const needle = query.trim().toLocaleLowerCase();
    const matches = needle
      ? rooms.filter((room) => name(room).toLocaleLowerCase().includes(needle))
      : rooms;
    return matches.slice(0, MAX_RESULTS);
  });

  // The name split around the first match, for highlighting. Lower-casing can
  // change the length of some text; such names are shown without a highlight.
  function parts(text: string): [string, string, string] {
    const needle = query.trim().toLocaleLowerCase();
    const lower = text.toLocaleLowerCase();
    const at = needle ? lower.indexOf(needle) : -1;
    if (at < 0 || lower.length !== text.length) return [text, '', ''];
    return [text.slice(0, at), text.slice(at, at + needle.length), text.slice(at + needle.length)];
  }

  $effect(() => {
    void query;
    active = 0;
  });

  // Focus goes back to where the palette was opened from when it closes.
  onMount(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.showModal();
    return () => {
      dialog?.close();
      previous?.focus();
    };
  });

  function open(roomId: string) {
    onselect(roomId);
    onclose();
  }

  function keydown(event: KeyboardEvent) {
    if (event.key === 'ArrowDown' && results.length > 0) {
      event.preventDefault();
      active = (active + 1) % results.length;
    } else if (event.key === 'ArrowUp' && results.length > 0) {
      event.preventDefault();
      active = (active - 1 + results.length) % results.length;
    } else if (event.key === 'Enter' && !event.isComposing) {
      event.preventDefault();
      const room = results[active];
      if (room) open(room.roomId);
    }
  }

  $effect(() => {
    document.getElementById('jump-option-' + active)?.scrollIntoView({ block: 'nearest' });
  });

  // Escape and clicks on the scrim close the palette.
  function cancel(event: Event) {
    event.preventDefault();
    onclose();
  }
</script>

<dialog
  class="palette"
  aria-label="Jump to a room"
  bind:this={dialog}
  oncancel={cancel}
  onclick={(event) => {
    if (event.target === dialog) onclose();
  }}
>
  <div class="palette-input">
    <Icon name="search" />
    <input
      type="text"
      role="combobox"
      aria-label="Room name"
      aria-expanded="true"
      aria-controls="jump-results"
      aria-autocomplete="list"
      aria-activedescendant={results.length > 0 ? 'jump-option-' + active : undefined}
      placeholder="Jump to a room…"
      maxlength="256"
      spellcheck="false"
      autocomplete="off"
      bind:value={query}
      onkeydown={keydown}
    />
    <kbd>esc</kbd>
  </div>
  <div class="palette-results">
    <p class="section-label" id="jump-label">Rooms</p>
    {#if results.length === 0}
      <p class="empty">No rooms match.</p>
    {/if}
    <ul id="jump-results" role="listbox" aria-labelledby="jump-label">
      {#each results as room, index (room.roomId)}
        {@const [before, match, after] = parts(name(room))}
        <li
          id={'jump-option-' + index}
          role="option"
          tabindex="-1"
          aria-selected={index === active}
          onmousemove={() => (active = index)}
          onclick={() => open(room.roomId)}
          onkeydown={keydown}
        >
          <EncryptionIcon encryption={room.encryption} />
          <Icon name="hash" class="hash" />
          <bdi class="room-name">{before}{#if match}<mark>{match}</mark>{/if}{after}</bdi>
          <span class="enter" aria-hidden="true">↵</span>
        </li>
      {/each}
    </ul>
  </div>
  <div class="palette-footer" aria-hidden="true">
    <span><kbd>↑↓</kbd> move</span>
    <span><kbd>↵</kbd> open</span>
    <span><kbd>esc</kbd> close</span>
  </div>
</dialog>
