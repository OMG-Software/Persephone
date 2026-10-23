<script lang="ts">
  import type { RoomSummary } from '../../crates/contracts/generated/contracts';
  import EncryptionIcon from './EncryptionIcon.svelte';
  import Icon from './Icon.svelte';

  let {
    rooms,
    truncated,
    selected,
    onselect
  }: {
    rooms: RoomSummary[];
    truncated: boolean;
    selected: string | null;
    onselect: (roomId: string) => void;
  } = $props();

  const unreadLabel = (count: number) => (count > 999 ? '999+' : String(count));
</script>

<nav aria-label="Rooms">
  <p class="section-label">Rooms</p>
  <ul>
    {#each rooms as room (room.roomId)}
      <li>
        <button
          type="button"
          class="room-row"
          class:unread={room.unreadCount > 0}
          aria-current={selected === room.roomId ? 'true' : undefined}
          onclick={() => onselect(room.roomId)}
        >
          <EncryptionIcon encryption={room.encryption} />
          <Icon name="hash" class="hash" />
          <bdi class="room-name">{room.displayName ?? room.roomId}</bdi>
          {#if room.unreadCount > 0}
            <span class="badge" aria-label={unreadLabel(room.unreadCount) + ' unread'}
              >{unreadLabel(room.unreadCount)}</span
            >
          {/if}
        </button>
      </li>
    {/each}
  </ul>
  {#if rooms.length === 0}
    <p class="note">No rooms yet.</p>
  {/if}
  {#if truncated}
    <p class="note">Showing the first {rooms.length} rooms.</p>
  {/if}
</nav>
