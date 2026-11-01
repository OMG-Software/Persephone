<script lang="ts">
  import type { RoomPreference, RoomSummary } from '../../crates/contracts/generated/contracts';
  import { groupRooms } from './roomGroups';
  import EncryptionIcon from './EncryptionIcon.svelte';
  import Icon from './Icon.svelte';

  let {
    rooms,
    truncated,
    selected,
    preferences = [],
    onselect
  }: {
    rooms: RoomSummary[];
    truncated: boolean;
    selected: string | null;
    preferences?: RoomPreference[];
    onselect: (roomId: string) => void;
  } = $props();

  const unreadLabel = (count: number) => (count > 999 ? '999+' : String(count));
  const groups = $derived(groupRooms(rooms, preferences));
</script>

<nav aria-label="Rooms">
  {#each groups as group, index (index)}
    <p class="section-label"><bdi>{group.name}</bdi></p>
    <ul aria-label={group.name}>
    {#each group.rooms as room (room.roomId)}
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
  {/each}
  {#if rooms.length === 0}
    <p class="note">No rooms yet.</p>
  {/if}
  {#if truncated}
    <p class="note">Showing the first {rooms.length} rooms.</p>
  {/if}
</nav>
