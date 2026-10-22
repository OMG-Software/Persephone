<script lang="ts">
  import type { RoomSummary } from '../../crates/contracts/generated/contracts';

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
  <ul>
    {#each rooms as room (room.roomId)}
      <li>
        <button
          type="button"
          aria-current={selected === room.roomId ? 'true' : undefined}
          onclick={() => onselect(room.roomId)}
        >
          <bdi class="room-name">{room.displayName ?? room.roomId}</bdi>
          {#if room.encryption === 'not_encrypted'}
            <span class="note">Not encrypted</span>
          {/if}
          {#if room.unreadCount > 0}
            <span class="badge" aria-label={unreadLabel(room.unreadCount) + ' unread'}
              >{unreadLabel(room.unreadCount)}</span
            >
          {/if}
        </button>
      </li>
    {/each}
  </ul>
  {#if truncated}
    <p class="note">Showing the first {rooms.length} rooms.</p>
  {/if}
</nav>
