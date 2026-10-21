<script lang="ts">
  import { onMount } from 'svelte';
  import markUrl from './assets/mark.svg';
  import RoomList from './RoomList.svelte';
  import SignIn from './SignIn.svelte';
  import Timeline from './Timeline.svelte';
  import { mergeNewest } from './timeline';
  import {
    appStatus,
    lockSession,
    roomList,
    signIn,
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
  // Only the newest room-list request may replace the list: an older reply
  // that finishes last must not bring back a stale list.
  let roomsRequest = 0;
  let signingIn = $state(false);
  // Advanced on every session change. Replies to calls made before the change
  // are discarded, so nothing from a locked session is shown again.
  let sessionGeneration = 0;

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
    restart_required: 'Locked: restart Persephone to unlock',
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

  // Signed in, signed out, or no profile (synthetic data only).
  const profileOpen = $derived(status?.store === 'protected');
  const signedIn = $derived(profileOpen && status?.account.state === 'signed_in');

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
    const generation = sessionGeneration;
    let reply: AppStatus;
    try {
      reply = await appStatus();
    } catch {
      if (generation === sessionGeneration) unavailable = true;
      return;
    }
    if (generation !== sessionGeneration) return;
    status = reply;
    if (reply.store !== 'protected' && reply.store !== 'not_configured') return;
    const request = ++roomsRequest;
    try {
      const list = await roomList();
      if (generation === sessionGeneration && request === roomsRequest) rooms = list;
    } catch (reason) {
      if (generation === sessionGeneration) error = describe(reason);
    }
  }

  // The session changed: drop everything shown from it. Pending fetches see
  // the cleared selection and discard their results.
  function reset() {
    sessionGeneration += 1;
    selected = null;
    rooms = null;
    items = [];
    nextCursor = null;
    error = null;
    // The host already ended the room subscription.
    live = null;
    // Operations from the old session no longer update these.
    loading = false;
    opening = false;
    changedAgain = false;
    load();
  }

  // Rooms were added or changed: fetch the list again, keeping the selection.
  async function refreshRooms() {
    if (!status || (status.store !== 'protected' && status.store !== 'not_configured')) return;
    const generation = sessionGeneration;
    const request = ++roomsRequest;
    try {
      const list = await roomList();
      if (generation === sessionGeneration && request === roomsRequest) rooms = list;
    } catch {
      // The next change or session event tries again.
    }
  }

  async function startSignIn(homeserver: string) {
    if (signingIn) return;
    signingIn = true;
    const generation = sessionGeneration;
    try {
      const account = await signIn(homeserver);
      if (generation === sessionGeneration && status) status = { ...status, account };
    } catch (reason) {
      if (generation === sessionGeneration) {
        error =
          String(reason) === 'invalid_argument'
            ? 'Use the homeserver\'s base address, such as https://matrix.example.org, with no path, query or fragment.'
            : describe(reason);
      }
    } finally {
      signingIn = false;
    }
  }

  async function toggleLock() {
    if (!status || switching) return;
    switching = true;
    const generation = sessionGeneration;
    try {
      const store = status.store === 'protected' || status.store === 'locking'
        ? await lockSession()
        : await unlockSession();
      // A session change during the call already reloaded the status.
      if (generation === sessionGeneration && status) status = { ...status, store };
    } catch (reason) {
      if (generation === sessionGeneration) error = describe(reason);
    } finally {
      switching = false;
    }
  }

  onMount(async () => {
    // Register for session changes before loading anything from the session:
    // the host does not replay a change the page was not yet listening for.
    // Without the channel the page could keep showing a locked session, so it
    // loads nothing.
    try {
      await watchSession(reset, refreshRooms);
    } catch {
      unavailable = true;
      return;
    }
    load();
  });

  // A room operation's results apply only to the same room in the same
  // session: the room ID alone matches again when a room is reopened after a
  // lock and unlock.
  const current = (roomId: string, generation: number) =>
    selected === roomId && generation === sessionGeneration;

  function follow(roomId: string, generation: number): Promise<unknown> {
    const task = async () => {
      if (live) {
        const previous = live;
        live = null;
        await previous.stop().catch(() => {});
      }
      if (!current(roomId, generation)) return;
      try {
        const stop = await subscribeRoom(roomId, () => refresh(roomId, generation));
        if (current(roomId, generation)) {
          live = { roomId, stop };
        } else {
          await stop().catch(() => {});
        }
      } catch (reason) {
        // The room still opens; it just will not update by itself.
        if (current(roomId, generation)) error = describe(reason);
      }
    };
    subscriptions = subscriptions.then(task, task);
    return subscriptions;
  }

  async function open(roomId: string) {
    const generation = sessionGeneration;
    selected = roomId;
    items = [];
    nextCursor = null;
    error = null;
    loading = true;
    opening = true;
    changedAgain = false;
    try {
      // Subscribe first, so a change after this fetch is not missed.
      await follow(roomId, generation);
      if (!current(roomId, generation)) return;
      const page = await timelinePage(roomId, null);
      if (!current(roomId, generation)) return;
      items = page.items;
      nextCursor = page.nextCursor;
    } catch (reason) {
      if (current(roomId, generation)) error = describe(reason);
    } finally {
      if (current(roomId, generation)) {
        loading = false;
        opening = false;
        if (changedAgain) refresh(roomId, generation);
      }
    }
  }

  async function refresh(roomId: string, generation: number) {
    if (!current(roomId, generation)) return;
    if (opening || refreshing) {
      changedAgain = true;
      return;
    }
    refreshing = true;
    try {
      do {
        changedAgain = false;
        const page = await timelinePage(roomId, null);
        if (!current(roomId, generation)) return;
        items = mergeNewest(items, page.items);
      } while (changedAgain);
    } catch (reason) {
      if (current(roomId, generation)) error = describe(reason);
    } finally {
      refreshing = false;
      // A change in a room opened while this fetch ran.
      if (changedAgain && selected && !opening) refresh(selected, sessionGeneration);
    }
  }

  async function older() {
    if (!selected || !nextCursor) return;
    const roomId = selected;
    const generation = sessionGeneration;
    loading = true;
    try {
      const page = await timelinePage(roomId, nextCursor);
      if (!current(roomId, generation)) return;
      items = [...page.items, ...items];
      nextCursor = page.nextCursor;
    } catch (reason) {
      if (current(roomId, generation)) error = describe(reason);
    } finally {
      if (current(roomId, generation)) loading = false;
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
          {#if signedIn && status.account.userId}
            · Signed in as <bdi>{status.account.userId}</bdi>
          {/if}
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
    {#if profileOpen && !signedIn}
      <SignIn account={status!.account} busy={signingIn} onsignin={startSignIn} />
    {:else if rooms}
      <RoomList rooms={rooms.rooms} truncated={rooms.truncated} {selected} onselect={open} />
    {/if}
    {#if profileOpen && !signedIn}
      {#if error}
        <p role="alert" class="error">{error}</p>
      {/if}
    {:else if selectedRoom}
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
        {#if status?.store === 'restart_required'}
          <p>The session is locked. Restart Persephone to unlock it.</p>
        {:else if status && status.store !== 'protected' && status.store !== 'not_configured'}
          <p>The session is locked. Nothing from it is shown.</p>
        {:else if signedIn}
          <p>Select a room.</p>
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
