<script lang="ts">
  import { onMount } from 'svelte';
  import markUrl from './assets/mark.svg';
  import RoomList from './RoomList.svelte';
  import Timeline from './Timeline.svelte';
  import {
    appStatus,
    roomList,
    timelinePage,
    type AppStatus,
    type RoomList as Rooms,
    type TimelineItem
  } from './bridge';

  let status = $state<AppStatus | null>(null);
  let unavailable = $state(false);
  let rooms = $state<Rooms | null>(null);
  let selected = $state<string | null>(null);
  let items = $state<TimelineItem[]>([]);
  let nextCursor = $state<string | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);

  const selectedRoom = $derived(rooms?.rooms.find((room) => room.roomId === selected) ?? null);

  // Error codes from the host map to fixed messages; nothing else is shown.
  const messages: Record<string, string> = {
    not_found: 'That room or page no longer exists.',
    invalid_argument: 'That request was not valid.'
  };
  const describe = (reason: unknown) =>
    messages[String(reason)] ?? 'The Persephone core is unavailable.';

  onMount(() => {
    appStatus()
      .then((reply) => (status = reply))
      .catch(() => (unavailable = true));
    roomList()
      .then((reply) => (rooms = reply))
      .catch((reason) => (error = describe(reason)));
  });

  async function open(roomId: string) {
    selected = roomId;
    items = [];
    nextCursor = null;
    error = null;
    loading = true;
    try {
      const page = await timelinePage(roomId, null);
      if (selected !== roomId) return;
      items = page.items;
      nextCursor = page.nextCursor;
    } catch (reason) {
      error = describe(reason);
    } finally {
      loading = false;
    }
  }

  async function older() {
    if (!selected || !nextCursor) return;
    const roomId = selected;
    loading = true;
    try {
      const page = await timelinePage(roomId, nextCursor);
      if (selected !== roomId) return;
      items = [...page.items, ...items];
      nextCursor = page.nextCursor;
    } catch (reason) {
      error = describe(reason);
    } finally {
      loading = false;
    }
  }
</script>

<main>
  <header>
    <img src={markUrl} alt="" width="36" height="36" />
    <div>
      <h1>Persephone</h1>
      <p role="status">
        {#if status}
          Rust host {status.version} · {status.milestone}
        {:else if unavailable}
          Rust host unavailable
        {:else}
          Connecting to the Rust host…
        {/if}
      </p>
    </div>
  </header>

  <div class="layout">
    {#if rooms}
      <RoomList rooms={rooms.rooms} truncated={rooms.truncated} {selected} onselect={open} />
    {/if}
    {#if selectedRoom}
      <Timeline
        title={selectedRoom.displayName ?? selectedRoom.roomId}
        {items}
        hasOlder={nextCursor !== null}
        {loading}
        {error}
        onolder={older}
      />
    {:else}
      <section class="timeline" aria-label="No room selected">
        <p>Select a room. All rooms and messages here are synthetic test data.</p>
        {#if error}
          <p role="alert" class="error">{error}</p>
        {/if}
      </section>
    {/if}
  </div>
</main>
