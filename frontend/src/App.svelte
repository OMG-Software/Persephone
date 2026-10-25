<script lang="ts">
  import { onMount } from 'svelte';
  import markUrl from './assets/mark.svg';
  import Composer from './Composer.svelte';
  import Icon from './Icon.svelte';
  import IdentityChanges from './IdentityChanges.svelte';
  import JumpPalette from './JumpPalette.svelte';
  import Security from './Security.svelte';
  import Settings, { type SettingsTab } from './Settings.svelte';
  import Sidebar from './Sidebar.svelte';
  import SignIn from './SignIn.svelte';
  import Timeline from './Timeline.svelte';
  import TipButton from './TipButton.svelte';
  import Verification from './Verification.svelte';
  import { LOCK_HELP, RESET_HELP } from './help';
  import { isolate } from './people';
  import { attachErrorText, sendImageErrorText, type ImageDraft } from './image';
  import { securityErrorText } from './securityText';
  import { mergeNewest } from './timeline';
  import {
    allowUnencrypted,
    appStatus,
    discardImage,
    discardMessage,
    pickImage,
    sendImage,
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
    contactStatus,
    verifyContact,
    identityChanges,
    acceptIdentity,
    signOut,
    roomList,
    signIn,
    subscribeRoom,
    timelinePage,
    resetProfile,
    unlockSession,
    watchSession,
    type AppStatus,
    type ContactStatus,
    type RoomIdentityChanges,
    type PendingMessage,
    type RoomList as Rooms,
    type SecurityStatus,
    type StoreState,
    type TimelineItem
  } from './bridge';

  let status = $state<AppStatus | null>(null);
  let security = $state<SecurityStatus | null>(null);
  let securityBusy = $state(false);
  // Why the last security action failed. Kept apart from `error`, so a later
  // success clears it without hiding a room's error.
  let securityFailure = $state<string | null>(null);
  // A security reply replaces the shown one only if it is newer, as for rooms.
  let securityRequest = 0;
  let securityShown = 0;
  // The other person in the selected direct-message room, if it is one.
  let contact = $state<ContactStatus | null>(null);
  // Members of the selected room whose identity changed (ADR-0007), and
  // whether the last attempt to find out failed.
  let changes = $state<RoomIdentityChanges | null>(null);
  let changesFailed = $state(false);
  // The room whose contact this page last asked to verify, and the room in
  // which it last asked to accept a new identity: the core reports why that
  // failed in the security status, shown in that room, each in its own place.
  let contactAsked = $state<string | null>(null);
  let identityAsked = $state<string | null>(null);
  // Messages sent from this device in the selected room.
  let pending = $state<PendingMessage[]>([]);
  // Sending in a room that is not encrypted asks once first (ADR-0005); the
  // core also refuses until the consent is recorded.
  // `image` is true when the question is about sending the attached image.
  let confirmingPlain = $state<{
    roomId: string;
    body: string | null;
    retryId: number | null;
    image: boolean;
  } | null>(null);
  // A picked image waiting for Send or Cancel. The core holds it under the
  // preview's handle until it is sent, discarded, or the session locks.
  let imageDraft = $state<ImageDraft | null>(null);
  let attaching = $state(false);
  let sendingImage = $state(false);
  let imageError = $state<string | null>(null);
  let draftVersion = $state(0);
  // The composer's unsent text, kept here so it outlives the composer.
  let draft = $state('');
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
  // What the window shows besides the room: settings, and the jump palette.
  let view = $state<'chat' | 'settings'>('chat');
  let settingsTab = $state<SettingsTab>('security');
  let paletteOpen = $state(false);
  // A finished or cancelled verification stays on screen until this is its ID.
  let dismissedFlow = $state<string | null>(null);
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
    not_configured: 'Demo rooms with synthetic data: nothing is stored',
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

  // The room shell: signed in, or synthetic rooms with no profile.
  const shellOpen = $derived(status !== null && (signedIn || status.store === 'not_configured'));

  const selectedRoom = $derived(rooms?.rooms.find((room) => room.roomId === selected) ?? null);

  // A verification has its own screen while it runs, and until the person
  // continues after it ends.
  const flow = $derived(signedIn ? (security?.verification ?? null) : null);
  const showFlow = $derived(flow !== null && flow.flowId !== dismissedFlow);
  const chatShown = $derived(shellOpen && view === 'chat' && !showFlow);
  // Why the core could not carry out the last step of an open verification.
  const flowFailure = $derived(
    flow !== null &&
      flow.phase !== 'done' &&
      flow.phase !== 'cancelled' &&
      security?.operation === 'idle' &&
      security.error
      ? securityErrorText[security.error]
      : null
  );
  const contactFailure = $derived(
    contactAsked !== null &&
      contactAsked === selected &&
      security?.operation === 'idle' &&
      security.error
      ? securityErrorText[security.error]
      : null
  );
  // A refused request (`securityFailure`) or what the core reported after
  // asking in its own window.
  const identityFailure = $derived(
    identityAsked === null || identityAsked !== selected
      ? null
      : (securityFailure ??
          (security?.operation === 'idle' && security.error
            ? securityErrorText[security.error]
            : null))
  );

  // The jump shortcut as this system writes it.
  const mac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform);
  const shortcut = mac ? '⌘K' : 'Ctrl K';

  // Ctrl K (⌘K on macOS) opens the jump palette from the room view.
  function shortcutKey(event: KeyboardEvent) {
    if (!(mac ? event.metaKey : event.ctrlKey) || event.altKey || event.key.toLowerCase() !== 'k') {
      return;
    }
    if (chatShown && rooms && !paletteOpen) {
      event.preventDefault();
      paletteOpen = true;
    }
  }

  function openSettings(tab: SettingsTab) {
    paletteOpen = false;
    settingsTab = tab;
    view = 'settings';
  }

  function closeSettings() {
    confirmingSignOut = false;
    view = 'chat';
  }

  // After verifying this device, the next step is recovery if it is not set
  // up; after verifying a contact, the room they came from.
  function dismissFlow(flowId: string) {
    dismissedFlow = flowId;
    if (flow?.contact) return;
    if (security?.recovery === 'disabled' && security.canSetUpRecovery) openSettings('security');
  }

  // Room names are untrusted: isolated, so they cannot reorder the suffix.
  $effect(() => {
    const name = selectedRoom ? (selectedRoom.displayName ?? selectedRoom.roomId) : null;
    document.title = chatShown && name !== null ? isolate(name) + ' · Persephone' : 'Persephone';
  });

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
    view = 'chat';
    paletteOpen = false;
    dismissedFlow = null;
    draft = '';
    // Nothing from the old session stays in the header while the new status loads.
    status = null;
    security = null;
    securityBusy = false;
    securityFailure = null;
    pending = [];
    contact = null;
    changes = null;
    changesFailed = false;
    contactAsked = null;
    identityAsked = null;
    confirmingPlain = null;
    // The core dropped every image handle with the session.
    imageDraft = null;
    attaching = false;
    sendingImage = false;
    imageError = null;
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
    // A verification or an identity change may have changed how far a
    // contact is trusted, and whether sending in the room is paused.
    if (selected && generation === sessionGeneration) {
      loadContact(selected, generation);
      loadChanges(selected, generation);
    }
  }

  // The selected room's changed identities. Kept only while the same room in
  // the same session is selected; a failure shows nothing (the core still
  // refuses to send).
  // Ordered like the contact replies: an older reply never replaces a newer one.
  let changesRequest = 0;
  let changesShown = 0;
  async function loadChanges(roomId: string, generation: number) {
    const request = ++changesRequest;
    if (!signedIn || selectedRoom?.encryption !== 'encrypted') {
      changes = null;
      changesFailed = false;
      changesShown = request;
      return;
    }
    let reply: RoomIdentityChanges | null = null;
    try {
      reply = await identityChanges(roomId);
    } catch {
      // The core still refuses to send when it cannot tell; say so and
      // offer to look again rather than show nothing.
    }
    if (current(roomId, generation) && request > changesShown) {
      changes = reply;
      changesFailed = reply === null;
      changesShown = request;
    }
  }

  function verifyRoomContact(roomId: string) {
    startSecurity(() => verifyContact(roomId), { room: roomId, kind: 'contact' });
  }

  // Asks the core to accept a member's new identity; the room's list is
  // fetched again afterwards, since the change may be gone (`not_found`).
  async function acceptRoomIdentity(roomId: string, userId: string) {
    const generation = sessionGeneration;
    await startSecurity(
      () => acceptIdentity(roomId, userId),
      { room: roomId, kind: 'identity' },
      (reason) =>
        String(reason) === 'not_found' ? 'That identity change is no longer listed.' : describe(reason)
    );
    if (current(roomId, generation)) loadChanges(roomId, generation);
  }

  // The selected room's contact, for a direct-message room. Kept only while
  // the same room in the same session is selected.
  // A reply replaces the shown one only if it is newer, as for security: a
  // slow reply from when the room opened must not undo a verification.
  let contactRequest = 0;
  let contactShown = 0;
  async function loadContact(roomId: string, generation: number) {
    const request = ++contactRequest;
    if (!signedIn || !selectedRoom?.contact) {
      contact = null;
      contactShown = request;
      return;
    }
    let reply: ContactStatus | null = null;
    try {
      reply = await contactStatus(roomId);
    } catch {
      // Shown as no contact.
    }
    if (current(roomId, generation) && request > contactShown) {
      contact = reply;
      contactShown = request;
    }
  }

  // `asked` names the room whose contact this action verifies, or in which
  // it accepts a new identity; every other action clears both, so a later
  // failure is not shown in that room. `explain` turns a refusal into text.
  async function startSecurity(
    start: () => Promise<SecurityStatus>,
    asked: { room: string; kind: 'contact' | 'identity' } | null = null,
    explain: (reason: unknown) => string = describe
  ) {
    if (securityBusy) return;
    securityBusy = true;
    securityFailure = null;
    contactAsked = asked?.kind === 'contact' ? asked.room : null;
    identityAsked = asked?.kind === 'identity' ? asked.room : null;
    const generation = sessionGeneration;
    const request = ++securityRequest;
    try {
      const reply = await start();
      if (generation === sessionGeneration && request > securityShown) {
        security = reply;
        securityShown = request;
      }
    } catch (reason) {
      if (generation === sessionGeneration) securityFailure = explain(reason);
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
    // An attached image belongs to the room it was picked for.
    if (imageDraft && imageDraft.roomId !== roomId) cancelImage();
    imageError = null;
    selected = roomId;
    items = [];
    pending = [];
    contact = null;
    changes = null;
    changesFailed = false;
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
      if (signedIn) {
        loadPending(roomId, generation);
        loadContact(roomId, generation);
        loadChanges(roomId, generation);
      }
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
      confirmingPlain = { roomId, body, retryId: null, image: false };
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
    if (asked.image && imageDraft?.roomId === asked.roomId) {
      await queueImage(asked.roomId, imageDraft.preview.handle, generation);
    }
  }

  // Opens the core's file dialog. A draft that arrives after the room or the
  // session changed is dropped at once.
  async function attachImage() {
    const roomId = selected;
    if (!roomId || attaching || imageDraft) return;
    const generation = sessionGeneration;
    attaching = true;
    imageError = null;
    try {
      const pick = await pickImage(roomId);
      if (pick.kind !== 'draft') return;
      if (current(roomId, generation) && !imageDraft) {
        imageDraft = { roomId, preview: pick.preview, size: pick.size, mime: pick.mime };
      } else {
        discardImage(pick.preview.handle).catch(() => {});
      }
    } catch (reason) {
      if (current(roomId, generation)) imageError = attachErrorText(reason);
    } finally {
      if (generation === sessionGeneration) attaching = false;
    }
  }

  function cancelImage() {
    const draft = imageDraft;
    imageDraft = null;
    imageError = null;
    // A lock already dropped it; nothing else needs doing.
    if (draft) discardImage(draft.preview.handle).catch(() => {});
  }

  // Sending an image asks for consent in a room that is not encrypted, as
  // text does; the core also refuses until the consent is recorded.
  async function sendAttached() {
    const draft = imageDraft;
    if (!draft || draft.roomId !== selected || sendingImage) return;
    if (selectedRoom?.encryption === 'not_encrypted' && !confirmedRooms.has(draft.roomId)) {
      confirmingPlain = { roomId: draft.roomId, body: null, retryId: null, image: true };
      return;
    }
    await queueImage(draft.roomId, draft.preview.handle, sessionGeneration);
  }

  async function queueImage(roomId: string, handle: string, generation: number) {
    if (sendingImage) return;
    sendingImage = true;
    imageError = null;
    try {
      await sendImage(roomId, handle);
      if (!current(roomId, generation)) return;
      // The core took the draft into its outbox.
      if (imageDraft?.preview.handle === handle) imageDraft = null;
      await loadPending(roomId, generation);
    } catch (reason) {
      if (!current(roomId, generation)) return;
      if (String(reason) === 'not_found' && imageDraft?.preview.handle === handle) imageDraft = null;
      imageError = sendImageErrorText(reason);
    } finally {
      if (generation === sessionGeneration) sendingImage = false;
    }
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
      ? 'Verify this device in Settings, Security & encryption, to send encrypted messages.'
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


<svelte:window onkeydown={shortcutKey} />

{#snippet brand()}
  <div class="auth-brand">
    <img src={markUrl} alt="" width="32" height="32" />
    Persephone
  </div>
{/snippet}

{#snippet lockButton(above: boolean, iconOnly: boolean)}
  <!-- Not while the sign-out question is open: it would apply to a locked session. -->
  <TipButton
    id="lock-help"
    help={LOCK_HELP}
    {above}
    class={iconOnly ? 'icon-button' : 'secondary'}
    label={iconOnly ? (switching ? 'Working…' : 'Lock') : undefined}
    disabled={switching || confirmingSignOut}
    onclick={toggleLock}
  >
    {#if iconOnly}
      <Icon name="lock" />
    {:else}
      {switching ? 'Working…' : 'Lock'}
    {/if}
  </TipButton>
{/snippet}

<main>
  {#if !status}
    <div class="centred">
      <section class="auth-card" aria-label="Persephone">
        {@render brand()}
        <p role="status">
          {unavailable ? 'Rust host unavailable' : 'Connecting to the Rust host…'}
        </p>
      </section>
    </div>
  {:else if showFlow && flow}
    <div class="centred">
      <div class="centred-stack">
        <Verification
          {flow}
          busy={securityBusy}
          onaccept={(flowId) => startSecurity(() => acceptVerification(flowId))}
          onconfirm={(flowId) => startSecurity(() => confirmVerification(flowId))}
          oncancel={(flowId) => startSecurity(() => cancelVerification(flowId))}
          ondismiss={dismissFlow}
        />
        {#if securityFailure}
          <p role="alert" class="error">{securityFailure}</p>
        {:else if flowFailure}
          <p role="alert" class="error">{flowFailure}</p>
        {/if}
      </div>
    </div>
  {:else if profileOpen && !signedIn}
    <div class="centred">
      <div class="centred-stack">
        <SignIn account={status.account} busy={signingIn} onsignin={startSignIn} />
        {#if error}
          <p role="alert" class="error">{error}</p>
        {/if}
        {@render lockButton(false, false)}
      </div>
    </div>
  {:else if shellOpen && view === 'settings'}
    <Settings
      userId={signedIn ? status.account.userId : null}
      tab={settingsTab}
      ontab={(tab) => (settingsTab = tab)}
      onback={closeSettings}
    >
      {#if settingsTab === 'security'}
        {#if signedIn && security}
          <Security
            {security}
            busy={securityBusy}
            onsetup={() => startSecurity(setUpEncryption)}
            onrecover={() => startSecurity(recoverWithKey)}
            onsetuprecovery={() => startSecurity(setUpRecovery)}
            onverify={() => startSecurity(verifyThisDevice)}
          />
        {:else}
          <section class="settings-page" aria-labelledby="security-heading">
            <h2 id="security-heading">Security &amp; encryption</h2>
            <p class="lede">
              {signedIn
                ? "Checking this account's encryption…"
                : 'No account is signed in. All rooms and messages here are synthetic test data.'}
            </p>
          </section>
        {/if}
        {#if securityFailure}
          <p role="alert" class="error">{securityFailure}</p>
        {/if}
      {:else}
        <section class="settings-page" aria-labelledby="session-heading">
          <h2 id="session-heading">Session</h2>
          <div class="panel">
            <p role="status">
              Rust host {status.version} · {status.milestone} · {storeText[status.store] ??
                'Local store state unknown'}
              {#if signedIn && status.account.userId}
                · Signed in as <bdi>{status.account.userId}</bdi>
              {/if}
            </p>
          </div>
          {#if status.store === 'protected' || status.store === 'locking'}
            <div class="panel">
              <h3>Lock</h3>
              <p>{LOCK_HELP}</p>
              <div class="actions">{@render lockButton(false, false)}</div>
            </div>
          {/if}
          {#if signedIn}
            <div class="panel">
              <h3>Sign out</h3>
              {#if confirmingSignOut}
                <div class="confirm" role="alertdialog" aria-labelledby="sign-out-question">
                  <p id="sign-out-question">
                    Sign out and delete this device's keys and local data? Encrypted history this
                    device could read may not be readable again.
                  </p>
                  <button type="button" disabled={switching} class="danger" onclick={confirmSignOut}>
                    Sign out
                  </button>
                  <button
                    type="button"
                    disabled={switching}
                    class="secondary"
                    onclick={() => (confirmingSignOut = false)}
                  >
                    Cancel
                  </button>
                </div>
              {:else}
                <p>Signs this device out of the homeserver and deletes its keys and local data.</p>
                <div class="actions">
                  <button
                    type="button"
                    disabled={switching}
                    class="danger"
                    onclick={() => (confirmingSignOut = true)}
                  >
                    Sign out
                  </button>
                </div>
              {/if}
            </div>
          {/if}
          {#if error}
            <p role="alert" class="error">{error}</p>
          {/if}
        </section>
      {/if}
    </Settings>
  {:else if shellOpen}
    <div class="shell">
      <Sidebar
        {rooms}
        {selected}
        userId={signedIn ? status.account.userId : null}
        {shortcut}
        onselect={open}
        onsearch={() => (paletteOpen = rooms !== null)}
        onsettings={() => openSettings('security')}
      >
        {#snippet notice()}
          {#if signedIn && (security?.identity === 'none' || security?.identity === 'unverified')}
            <div class="device-notice">
              <p>
                {security.identity === 'none'
                  ? 'Encryption is not set up for this account.'
                  : 'This device is not verified.'}
              </p>
              <button type="button" class="secondary" onclick={() => openSettings('security')}>
                {security.identity === 'none' ? 'Set up encryption' : 'Verify this device'}
              </button>
            </div>
          {/if}
        {/snippet}
        {#snippet lock()}
          {#if status?.store === 'protected' || status?.store === 'locking'}
            {@render lockButton(true, true)}
          {/if}
        {/snippet}
      </Sidebar>
      <div class="pane">
        {#if selectedRoom}
          <Timeline
            title={selectedRoom.displayName ?? selectedRoom.roomId}
            roomId={selectedRoom.roomId}
            encryption={selectedRoom.encryption}
            {items}
            hasOlder={nextCursor !== null}
            {loading}
            {error}
            onolder={older}
            {contact}
            canVerify={security?.identity === 'verified'}
            verifying={securityBusy || (flow !== null && flow.phase !== 'done' && flow.phase !== 'cancelled')}
            onverify={() => verifyRoomContact(selectedRoom.roomId)}
          />
          {#if contactFailure}
            <p role="alert" class="error contact-error">{contactFailure}</p>
          {/if}
          {#if signedIn && changesFailed}
            <div class="error contact-error" role="alert">
              Persephone could not check whether anyone's identity changed in this room, so it
              sends nothing here for now.
              <button
                type="button"
                class="secondary"
                onclick={() => loadChanges(selectedRoom.roomId, sessionGeneration)}>Try again</button
              >
            </div>
          {/if}
          {#if signedIn && changes && (changes.changes.length > 0 || changes.truncated)}
            <IdentityChanges
              {changes}
              contact={contact?.userId ?? null}
              canVerify={security?.identity === 'verified'}
              busy={securityBusy || security?.operation !== 'idle'}
              confirming={security?.operation === 'waiting_for_confirmation'}
              failure={identityFailure}
              onaccept={(userId) => acceptRoomIdentity(selectedRoom.roomId, userId)}
              onverify={() => verifyRoomContact(selectedRoom.roomId)}
            />
          {/if}
          {#if signedIn}
            {#if confirmingPlain}
              <div class="confirm" role="alertdialog" aria-labelledby="plain-question">
                <p id="plain-question">
                  This room is not encrypted: the homeserver and anyone with access to it can read
                  what you send here. {confirmingPlain.image
                    ? 'Send unencrypted messages in this room, starting with this image?'
                    : 'Send unencrypted messages in this room?'}
                </p>
                <button type="button" class="danger" onclick={confirmPlain}>Send unencrypted</button>
                <button type="button" class="secondary" onclick={() => (confirmingPlain = null)}>
                  Cancel
                </button>
              </div>
            {/if}
            <Composer
              pending={visiblePending}
              blocked={sendBlocked}
              encryption={selectedRoom.encryption}
              roomName={selectedRoom.displayName ?? selectedRoom.roomId}
              bind:draft
              {draftVersion}
              onsend={sendText}
              onretry={(localId) => changeMessage(retryMessage, localId)}
              ondiscard={(localId) => changeMessage(discardMessage, localId)}
              onallow={(localId) =>
                (confirmingPlain = { roomId: selectedRoom.roomId, body: null, retryId: localId, image: false })}
              {imageDraft}
              {attaching}
              {sendingImage}
              {imageError}
              onattach={attachImage}
              onsendimage={sendAttached}
              oncancelimage={cancelImage}
            />
          {/if}
        {:else}
          <section class="empty-pane" aria-label="No room selected">
            <p>
              {signedIn
                ? 'Select a room.'
                : 'Select a room. All rooms and messages here are synthetic test data.'}
            </p>
            {#if error}
              <p role="alert" class="error">{error}</p>
            {/if}
          </section>
        {/if}
      </div>
      {#if paletteOpen && rooms}
        <JumpPalette rooms={rooms.rooms} onselect={open} onclose={() => (paletteOpen = false)} />
      {/if}
    </div>
  {:else}
    <!-- Locked, locking or signed out: nothing from the session is shown. -->
    <div class="centred">
      <section class="auth-card" aria-labelledby="locked-heading">
        {@render brand()}
        <h1 id="locked-heading">
          {status.store === 'restart_required' && status.account.state === 'signed_out'
            ? 'Signed out'
            : 'Locked'}
        </h1>
        <p role="status">{storeText[status.store] ?? 'Local store state unknown'}</p>
        {#if status.store === 'restart_required' && status.account.state === 'signed_out' && status.account.error === 'failed'}
          <p role="alert" class="error">
            Signing out did not finish: this device's local data could not be deleted. Restart
            Persephone and sign out again.
          </p>
        {:else if status.store === 'restart_required' && status.account.state === 'signed_out'}
          <p class="auth-note">
            Signed out, and this device's keys were deleted. Restart Persephone to sign in again;
            the rest of this device's local data is deleted as it starts.
          </p>
          {#if status.account.error === 'session_not_revoked'}
            <p role="alert" class="error">
              The homeserver could not be reached, so the session may stay active there until it
              expires. Revoke it from another device if you need to.
            </p>
          {/if}
        {:else if status.store === 'restart_required'}
          <p class="auth-note">The session is locked. Restart Persephone to unlock it.</p>
        {:else}
          <p class="auth-note">The session is locked. Nothing from it is shown.</p>
        {/if}
        <div class="actions">
          {#if status.store === 'locking'}
            {@render lockButton(false, false)}
          {:else if unlockable.includes(status.store)}
            <button type="button" class="primary" disabled={switching} onclick={toggleLock}>
              {switching ? 'Unlocking…' : 'Unlock'}
            </button>
          {/if}
          {#if resettable.includes(status.store)}
            <TipButton id="reset-help" help={RESET_HELP} disabled={switching} onclick={startReset}>
              Reset profile…
            </TipButton>
          {/if}
        </div>
        {#if error}
          <p role="alert" class="error">{error}</p>
        {/if}
      </section>
    </div>
  {/if}
</main>
