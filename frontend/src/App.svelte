<script lang="ts">
  import { onMount } from 'svelte';
  import markUrl from './assets/mark.svg';
  import RoomList from './RoomList.svelte';
  import Timeline from './Timeline.svelte';
  import { mergeNewest } from './timeline';
  import {
    appStatus,
    lockSession,
    roomList,
    subscribeRoom,
    timelinePage,
    unlockSession,
    watchSession,
    type AppStatus,
    type RoomList as Rooms,
    type StoreState,
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
  let switching = $state(false);

  // The one room with live updates, and a queue that keeps subscribe and
  // unsubscribe calls in order: the host runs commands concurrently.
  let live: { roomId: string; stop: () => Promise<void> } | null = null;
  let subscriptions: Promise<unknown> = Promise.resolve();
  // A change that arrives while a fetch is running is applied after it.
  let opening = false;
  let refreshing = false;
  let changedAgain = false;

  // Fixed text per store state; the core sends codes only.
  const storeText: Record<StoreState, string> = {
    not_configured: 'No local profile: nothing is stored',
    protected: 'Local store protected',
    locked: 'Local store locked',
    locking: 'Locking: work in progress is finishing',
    lock_failed: 'Local store did not close cleanly; restart Persephone',
    key_missing: 'Local store locked: its key is missing',
    key_unreadable: 'Local store locked: its key cannot be unlocked',
    key_rejected: 'Local store locked: the key does not match',
    profile_in_use: 'Local store locked: open in another window',
    store_unavailable: 'Local store unavailable',
    unsupported: 'Local store unavailable on this system'
  };

  // States the core can try to unlock from; the others need a restart or
  // have nothing to unlock.
  const unlockable: StoreState[] = [
    'locked',
    'key_missing',
    'key_unreadable',
    'key_rejected',
    'profile_in_use',
    'store_unavailable'
  ];

  const selectedRoom = $derived(rooms?.rooms.find((room) => room.roomId === selected) ?? null);

  // Error codes from the host map to fixed messages; nothing else is shown.
  const messages: Record<string, string> = {
    not_found: 'That room or page no longer exists.',
    limit_exceeded: 'Too many rooms are open for live updates.',
    invalid_argument: 'That request was not valid.',
    locked: 'The session is locked.'
  };
  const describe = (reason: unknown) =>
    messages[String(reason)] ?? 'The Persephone core is unavailable.';

  async function load() {
    try {
      status = await appStatus();
    } catch {
      unavailable = true;
      return;
    }
    if (status.store !== 'protected' && status.store !== 'not_configured') return;
    try {
      rooms = await roomList();
    } catch (reason) {
      error = describe(reason);
    }
  }

  // The session changed: drop everything shown from it. Pending fetches see
  // the cleared selection and discard their results.
  function reset() {
    selected = null;
    rooms = null;
    items = [];
    nextCursor = null;
    error = null;
    // The host already ended the room subscription.
    live = null;
    changedAgain = false;
    load();
  }

  async function toggleLock() {
    if (!status || switching) return;
    switching = true;
    try {
      const store = status.store === 'protected' || status.store === 'locking'
        ? await lockSession()
        : await unlockSession();
      status = { ...status, store };
    } catch (reason) {
      error = describe(reason);
    } finally {
      switching = false;
    }
  }

  onMount(() => {
    watchSession(reset).catch(() => {});
    load();
  });

  function follow(roomId: string): Promise<unknown> {
    const task = async () => {
      if (live) {
        const previous = live;
        live = null;
        await previous.stop().catch(() => {});
      }
      if (selected !== roomId) return;
      try {
        live = { roomId, stop: await subscribeRoom(roomId, () => refresh(roomId)) };
      } catch (reason) {
        // The room still opens; it just will not update by itself.
        error = describe(reason);
      }
    };
    subscriptions = subscriptions.then(task, task);
    return subscriptions;
  }

  async function open(roomId: string) {
    selected = roomId;
    items = [];
    nextCursor = null;
    error = null;
    loading = true;
    opening = true;
    changedAgain = false;
    try {
      // Subscribe first, so a change after this fetch is not missed.
      await follow(roomId);
      if (selected !== roomId) return;
      const page = await timelinePage(roomId, null);
      if (selected !== roomId) return;
      items = page.items;
      nextCursor = page.nextCursor;
    } catch (reason) {
      error = describe(reason);
    } finally {
      if (selected === roomId) {
        loading = false;
        opening = false;
        if (changedAgain) refresh(roomId);
      }
    }
  }

  async function refresh(roomId: string) {
    if (selected !== roomId) return;
    if (opening || refreshing) {
      changedAgain = true;
      return;
    }
    refreshing = true;
    try {
      do {
        changedAgain = false;
        const page = await timelinePage(roomId, null);
        if (selected !== roomId) return;
        items = mergeNewest(items, page.items);
      } while (changedAgain);
    } catch (reason) {
      error = describe(reason);
    } finally {
      refreshing = false;
      // A change in a room opened while this fetch ran.
      if (changedAgain && selected && !opening) refresh(selected);
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
          Rust host {status.version} · {status.milestone} · {storeText[status.store] ??
            'Local store state unknown'}
        {:else if unavailable}
          Rust host unavailable
        {:else}
          Connecting to the Rust host…
        {/if}
      </p>
    </div>
    {#if status && (status.store === 'protected' || status.store === 'locking')}
      <button type="button" class="session" disabled={switching} onclick={toggleLock}>
        {switching ? 'Locking…' : 'Lock'}
      </button>
    {:else if status && unlockable.includes(status.store)}
      <button type="button" class="session" disabled={switching} onclick={toggleLock}>
        {switching ? 'Unlocking…' : 'Unlock'}
      </button>
    {/if}
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
        {#if status && status.store !== 'protected' && status.store !== 'not_configured'}
          <p>The session is locked. Nothing from it is shown.</p>
        {:else}
          <p>Select a room. All rooms and messages here are synthetic test data.</p>
        {/if}
        {#if error}
          <p role="alert" class="error">{error}</p>
        {/if}
      </section>
    {/if}
  </div>
</main>
