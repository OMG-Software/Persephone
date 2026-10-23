<script lang="ts">
  import { onMount } from 'svelte';
  import markUrl from './assets/mark.svg';
  import Composer from './Composer.svelte';
  import RoomList from './RoomList.svelte';
  import Security from './Security.svelte';
  import SignIn from './SignIn.svelte';
  import Timeline from './Timeline.svelte';
  import TipButton from './TipButton.svelte';
  import { LOCK_HELP, RESET_HELP } from './help';
  import { mergeNewest } from './timeline';
  import {
    allowUnencrypted,
    appStatus,
    discardMessage,
    lockSession,
    pendingMessages,
    retryMessage,
    sendMessage,
    recoverWithKey,
    securityStatus,
    setUpEncryption,
    setUpRecovery,
    verifyThisDevice,
    acceptVerification,
    confirmVerification,
    cancelVerification,
    signOut,
    roomList,
    signIn,
    subscribeRoom,
    timelinePage,
    resetProfile,
    unlockSession,
    watchSession,
    type AppStatus,
    type PendingMessage,
    type RoomList as Rooms,
    type SecurityStatus,
    type StoreState,
    type TimelineItem
  } from './bridge';

  let status = $state<AppStatus | null>(null);
  let security = $state<SecurityStatus | null>(null);
  let securityBusy = $state(false);
  // A security reply replaces the shown one only if it is newer, as for rooms.
  let securityRequest = 0;
  let securityShown = 0;
  // Messages sent from this device in the selected room.
  let pending = $state<PendingMessage[]>([]);
  // Sending in a room that is not encrypted asks once first (ADR-0005); the
  // core also refuses until the consent is recorded.
  let confirmingPlain = $state<{ roomId: string; body: string | null; retryId: number | null } | null>(
    null
  );
  let draftVersion = $state(0);
  // Rooms confirmed while this page runs; the core keeps the consent itself.
  let confirmedRooms = new Set<string>();
  let unavailable = $state(false);
  let rooms = $state<Rooms | null>(null);
  let selected = $state<string | null>(null);
  let items = $state<TimelineItem[]>([]);
  let nextCursor = $state<string | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let switching = $state(false);
  // Sign-out deletes this device's keys, so it asks once more first.
  let confirmingSignOut = $state(false);
  // A room-list reply replaces the list only if it is newer than the one
  // shown: an older reply that finishes last cannot bring back a stale list,
  // and a failed newer request leaves the newest successful one in place.
  let roomsRequest = 0;
  let roomsShown = 0;
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
    unlocking: 'Unlocking: enter the passphrase in the Persephone window',
    lock_failed: 'Local store did not close cleanly; restart Persephone',
    restart_required: 'Locked: restart Persephone to unlock',
    key_missing: 'Local store locked: its key is missing',
    key_unreadable: 'Local store locked: its key cannot be unlocked',
    secret_rejected: 'Local store locked: the passphrase was wrong too many times',
    key_rejected: 'Local store locked: the key does not match',
    profile_in_use: 'Local store locked: open in another window',
    store_unavailable: 'Local store unavailable',
    unsupported: 'Local store unavailable on this system'
  };

  // States the core can try to unlock from; the others need a restart or
  // have nothing to unlock.
  const unlockable: StoreState[] = [
    'locked',
    'secret_rejected',
    'key_missing',
    'key_unreadable',
    'key_rejected',
    'profile_in_use',
    'store_unavailable'
  ];

  // Closed profiles the core may reset after a forgotten passphrase. The core
  // checks again and asks for confirmation in its own window.
  const resettable: StoreState[] = [
    'locked',
    'secret_rejected',
    'key_missing',
    'key_unreadable',
    'key_rejected',
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
    if (reply.account.state === 'signed_in') refreshSecurity();
    const request = ++roomsRequest;
    try {
      const list = await roomList();
      if (generation === sessionGeneration && request > roomsShown) {
        rooms = list;
        roomsShown = request;
      }
    } catch (reason) {
      if (generation === sessionGeneration) error = describe(reason);
    }
  }

  // The session changed: drop everything shown from it. Pending fetches see
  // the cleared selection and discard their results.
  function reset() {
    sessionGeneration += 1;
    // A question about the old session no longer applies.
    confirmingSignOut = false;
    // Nothing from the old session stays in the header while the new status loads.
    status = null;
    security = null;
    securityBusy = false;
    pending = [];
    confirmingPlain = null;
    confirmedRooms = new Set();
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

  // The account's encryption state changed: fetch it again. Nothing else
  // changes.
  async function refreshSecurity() {
    const generation = sessionGeneration;
    const request = ++securityRequest;
    try {
      const reply = await securityStatus();
      if (generation === sessionGeneration && request > securityShown) {
        security = reply;
        securityShown = request;
      }
    } catch {
      // The next security or session event tries again.
    }
  }

  async function startSecurity(start: () => Promise<SecurityStatus>) {
    if (securityBusy) return;
    securityBusy = true;
    const generation = sessionGeneration;
    const request = ++securityRequest;
    try {
      const reply = await start();
      if (generation === sessionGeneration && request > securityShown) {
        security = reply;
        securityShown = request;
      }
    } catch (reason) {
      if (generation === sessionGeneration) error = describe(reason);
    } finally {
      if (generation === sessionGeneration) securityBusy = false;
    }
  }

  // Rooms were added or changed: fetch the list again, keeping the selection.
  async function refreshRooms() {
    if (!status || (status.store !== 'protected' && status.store !== 'not_configured')) return;
    const generation = sessionGeneration;
    const request = ++roomsRequest;
    try {
      const list = await roomList();
      if (generation === sessionGeneration && request > roomsShown) {
        rooms = list;
        roomsShown = request;
      }
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

  async function confirmSignOut() {
    if (switching) return;
    confirmingSignOut = false;
    switching = true;
    const generation = sessionGeneration;
    try {
      const store = await signOut();
      if (generation === sessionGeneration && status) status = { ...status, store };
    } catch (reason) {
      if (generation === sessionGeneration) {
        error =
          String(reason) === 'limit_exceeded'
            ? 'Persephone is busy, so signing out did not start. Try again.'
            : describe(reason);
      }
    } finally {
      switching = false;
    }
  }

  async function toggleLock() {
    if (!status || switching || confirmingSignOut) return;
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

  async function startReset() {
    if (!status || switching) return;
    switching = true;
    const generation = sessionGeneration;
    try {
      const store = await resetProfile();
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
      await watchSession(reset, refreshRooms, refreshSecurity);
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
    pending = [];
    confirmingPlain = null;
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
      if (signedIn) loadPending(roomId, generation);
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
        if (signedIn) await loadPending(roomId, generation);
      } while (changedAgain);
    } catch (reason) {
      if (current(roomId, generation)) error = describe(reason);
    } finally {
      refreshing = false;
      // A change in a room opened while this fetch ran.
      if (changedAgain && selected && !opening) refresh(selected, sessionGeneration);
    }
  }

  // As for the room list: an older pending-list reply that finishes last
  // cannot replace a newer one (a late "queued" over "sent").
  let pendingRequest = 0;
  let pendingShown = 0;

  async function loadPending(roomId: string, generation: number) {
    const request = ++pendingRequest;
    try {
      const list = await pendingMessages(roomId);
      if (current(roomId, generation) && request > pendingShown) {
        pending = list;
        pendingShown = request;
      }
    } catch {
      // The next change of the room tries again.
    }
  }

  async function queue(roomId: string, body: string, generation: number): Promise<boolean> {
    try {
      await sendMessage(roomId, body);
      if (current(roomId, generation)) await loadPending(roomId, generation);
      return true;
    } catch (reason) {
      if (current(roomId, generation)) {
        error =
          String(reason) === 'limit_exceeded'
            ? 'Too many messages are waiting to send. Wait, or discard some.'
            : describe(reason);
      }
      return false;
    }
  }

  async function sendText(body: string): Promise<boolean> {
    const roomId = selected;
    if (!roomId) return false;
    if (selectedRoom?.encryption === 'not_encrypted' && !confirmedRooms.has(roomId)) {
      confirmingPlain = { roomId, body, retryId: null };
      return false;
    }
    return queue(roomId, body, sessionGeneration);
  }

  async function confirmPlain() {
    const asked = confirmingPlain;
    confirmingPlain = null;
    if (!asked) return;
    const generation = sessionGeneration;
    try {
      await allowUnencrypted(asked.roomId);
    } catch (reason) {
      if (current(asked.roomId, generation)) error = describe(reason);
      return;
    }
    if (!current(asked.roomId, generation)) return;
    confirmedRooms.add(asked.roomId);
    if (asked.body !== null && (await queue(asked.roomId, asked.body, generation))) {
      draftVersion += 1;
    }
    if (asked.retryId !== null) changeMessage(retryMessage, asked.retryId);
  }

  async function changeMessage(change: (localId: number) => Promise<void>, localId: number) {
    const roomId = selected;
    if (!roomId) return;
    const generation = sessionGeneration;
    try {
      await change(localId);
    } catch (reason) {
      if (current(roomId, generation)) error = describe(reason);
    }
    if (current(roomId, generation)) loadPending(roomId, generation);
  }

  // Messages the timeline already shows are not listed again.
  const visiblePending = $derived(
    pending.filter(
      (message) =>
        !(
          message.state === 'sent' &&
          items.some((item) => item.kind !== 'gap' && item.event_id === message.eventId)
        )
    )
  );
  const sendBlocked = $derived(
    selectedRoom?.encryption !== 'not_encrypted' && security?.identity !== 'verified'
      ? 'Verify this device in the Encryption panel to send encrypted messages.'
      : null
  );

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
    {#if signedIn && !confirmingSignOut}
      <button
        type="button"
        class="session"
        disabled={switching}
        onclick={() => (confirmingSignOut = true)}
      >
        Sign out
      </button>
    {/if}
    {#if confirmingSignOut}
      <div class="confirm" role="alertdialog" aria-labelledby="sign-out-question">
        <p id="sign-out-question">
          Sign out and delete this device's keys and local data? Encrypted history this device
          could read may not be readable again.
        </p>
        <button type="button" disabled={switching} onclick={confirmSignOut}>Sign out</button>
        <button type="button" disabled={switching} onclick={() => (confirmingSignOut = false)}>
          Cancel
        </button>
      </div>
    {/if}
    {#if status && (status.store === 'protected' || status.store === 'locking')}
      <!-- Not while the sign-out question is open: it would apply to a locked session. -->
      <TipButton
        id="lock-help"
        help={LOCK_HELP}
        disabled={switching || confirmingSignOut}
        onclick={toggleLock}
      >
        {switching ? 'Working…' : 'Lock'}
      </TipButton>
    {:else if status && unlockable.includes(status.store)}
      <button type="button" class="session" disabled={switching} onclick={toggleLock}>
        {switching ? 'Unlocking…' : 'Unlock'}
      </button>
    {/if}
    {#if status && resettable.includes(status.store)}
      <TipButton id="reset-help" help={RESET_HELP} disabled={switching} onclick={startReset}>
        Reset profile…
      </TipButton>
    {/if}
  </header>

  <div class="layout">
    {#if profileOpen && !signedIn}
      <SignIn account={status!.account} busy={signingIn} onsignin={startSignIn} />
    {:else if rooms}
      <div class="side">
        {#if signedIn && security}
          <Security
            {security}
            busy={securityBusy}
            onsetup={() => startSecurity(setUpEncryption)}
            onrecover={() => startSecurity(recoverWithKey)}
            onsetuprecovery={() => startSecurity(setUpRecovery)}
            onverify={() => startSecurity(verifyThisDevice)}
            onaccept={(flowId) => startSecurity(() => acceptVerification(flowId))}
            onconfirm={(flowId) => startSecurity(() => confirmVerification(flowId))}
            oncancel={(flowId) => startSecurity(() => cancelVerification(flowId))}
          />
        {/if}
        <RoomList rooms={rooms.rooms} truncated={rooms.truncated} {selected} onselect={open} />
      </div>
    {/if}
    {#if profileOpen && !signedIn}
      {#if error}
        <p role="alert" class="error">{error}</p>
      {/if}
    {:else if selectedRoom}
      <div class="room">
        <Timeline
          title={selectedRoom.displayName ?? selectedRoom.roomId}
          notEncrypted={selectedRoom.encryption === 'not_encrypted'}
          {items}
          hasOlder={nextCursor !== null}
          {loading}
          {error}
          onolder={older}
        />
        {#if signedIn}
          {#if confirmingPlain}
            <div class="confirm" role="alertdialog" aria-labelledby="plain-question">
              <p id="plain-question">
                This room is not encrypted: the homeserver and anyone with access to it can read
                what you send here. Send unencrypted messages in this room?
              </p>
              <button type="button" onclick={confirmPlain}>Send unencrypted</button>
              <button type="button" onclick={() => (confirmingPlain = null)}>Cancel</button>
            </div>
          {/if}
          <Composer
            pending={visiblePending}
            blocked={sendBlocked}
            notEncrypted={selectedRoom.encryption === 'not_encrypted'}
            {draftVersion}
            onsend={sendText}
            onretry={(localId) => changeMessage(retryMessage, localId)}
            ondiscard={(localId) => changeMessage(discardMessage, localId)}
            onallow={(localId) =>
              (confirmingPlain = { roomId: selectedRoom.roomId, body: null, retryId: localId })}
          />
        {/if}
      </div>
    {:else}
      <section class="timeline" aria-label="No room selected">
        {#if status?.store === 'restart_required' && status.account.state === 'signed_out' && status.account.error === 'failed'}
          <p role="alert" class="error">
            Signing out did not finish: this device's local data could not be deleted. Restart
            Persephone and sign out again.
          </p>
        {:else if status?.store === 'restart_required' && status.account.state === 'signed_out'}
          <p>
            Signed out, and this device's keys were deleted. Restart Persephone to sign in again;
            the rest of this device's local data is deleted as it starts.
          </p>
          {#if status.account.error === 'session_not_revoked'}
            <p role="alert" class="error">
              The homeserver could not be reached, so the session may stay active there until it
              expires. Revoke it from another device if you need to.
            </p>
          {/if}
        {:else if status?.store === 'restart_required'}
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
