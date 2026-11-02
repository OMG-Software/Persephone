<script lang="ts">
  import { onMount, tick } from 'svelte';
  import markUrl from './assets/mark.svg';
  import AccountPage from './AccountPage.svelte';
  import Accounts from './Accounts.svelte';
  import Composer from './Composer.svelte';
  import RoomTools from './RoomTools.svelte';
  import ConnectionHealth from './ConnectionHealth.svelte';
  import { jumpToDate, eventContext, setPinned, setRoomPreference, startConversation } from './bridge';
  import type { FocusedTimeline } from '../../crates/contracts/generated/contracts';
  import LocalSearch from './LocalSearch.svelte';
  import IgnoreUsers from './IgnoreUsers.svelte';
  import ReportAbuse from './ReportAbuse.svelte';
  import RoomOrganisation from './RoomOrganisation.svelte';
  import RoomGroups from './RoomGroups.svelte';
  import PublicRooms from './PublicRooms.svelte';
  import { ownServer } from './publicRooms.ts';
  import SavedDrafts from './SavedDrafts.svelte';
  import StoredRoomGroups from './StoredRoomGroups.svelte';
  import { DraftKeeper } from './drafts';
  import { findSearchContext } from './searchNavigation';
  import Devices from './Devices.svelte';
  import Icon from './Icon.svelte';
  import Menu, { type MenuItem } from './Menu.svelte';
  import MemberPanel from './MemberPanel.svelte';
  import {
    SIDEBAR_DEFAULT,
    SIDEBAR_MIN,
    clampSidebar,
    draggedSidebar,
    keyedSidebar,
    membersFit,
    sidebarLimit
  } from './layout';
  import IdentityChanges from './IdentityChanges.svelte';
  import InvitePerson from './InvitePerson.svelte';
  import JumpPalette from './JumpPalette.svelte';
  import NewConversation from './NewConversation.svelte';
  import NewRoom from './NewRoom.svelte';
  import Preferences from './Preferences.svelte';
  import ProfileSettings from './ProfileSettings.svelte';
  import RoomSettings from './RoomSettings.svelte';
  import Security from './Security.svelte';
  import Settings, { type SettingsTab } from './Settings.svelte';
  import Sidebar from './Sidebar.svelte';
  import SignIn, { type SignInForm } from './SignIn.svelte';
  import Timeline from './Timeline.svelte';
  import NewSessionsAlert from './NewSessions.svelte';
  import { changeText } from './accountChanges';
  import TipButton from './TipButton.svelte';
  import Verification from './Verification.svelte';
  import { LOCK_HELP, RESET_HELP } from './help';
  import { isolate } from './people';
  import { DEFAULT_SETTINGS, SettingsKeeper, isLockShortcut, textScale } from './preferences';
  import { OwnNameKeeper } from './ownName';
  import { attachErrorText, sendImageErrorText, type ImageDraft } from './image';
  import { securityErrorText } from './securityText';
  import {
    applyRelations,
    firstUnread,
    mergeNewest,
    newestEvent,
    quoteOf,
    type Quote
  } from './timeline';
  import { TypingNotifier } from './typing';
  import {
    accountSettings,
    allowUnencrypted,
    appStatus,
    discardImage,
    discardMessage,
    displayName,
    pickImage,
    sendImage,
    lockSession,
    pendingMessages,
    retryMessage,
    sendMessage,
    editMessage,
    deleteMessage,
    react,
    markRead,
    newSessions,
    acknowledgeSessions,
    removeDevice,
    roomActivity,
    setTyping,
    openLink,
    pickFile,
    recoverWithKey,
    securityStatus,
    setUpEncryption,
    setUpRecovery,
    verifyThisDevice,
    verifyDevice,
    changeRecoveryKey,
    acceptVerification,
    confirmVerification,
    cancelVerification,
    contactStatus,
    verifyContact,
    verifyMember as verifyRoomMember,
    identityChanges,
    acceptIdentity,
    signOut,
    roomList,
    roomDraft,
    saveRoomDraft,
    type RoomDraft,
    roomPreferences,
    type RoomPreference,
    type LocalSearchHit,
    inviteList,
    acceptInvite,
    declineInvite,
    signIn,
    signInWithOidc,
    register,
    subscribeRoom,
    timelinePage,
    resetProfile,
    unlockSession,
    watchSession,
    setAccountSettings,
    type AccountSettings,
    type AppStatus,
    type ContactStatus,
    type InviteList,
    type RoomIdentityChanges,
    type PendingMessage,
    type RoomActivity,
    type NewSessions,
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
  // The room whose contact this page last asked to verify, the room in which
  // it last asked to accept a new identity, and the session it last asked to
  // verify: the core reports why that failed in the security status, shown
  // in that room or on the Sessions page, each in its own place.
  let contactAsked = $state<string | null>(null);
  // The member of the selected room this page last asked to verify: when the
  // core finds no direct-message room with them, the member panel offers to
  // start one.
  let memberAsked = $state<{ room: string; user: string } | null>(null);
  let identityAsked = $state<string | null>(null);
  let deviceAsked = $state<string | null>(null);
  // Messages sent from this device in the selected room.
  let pending = $state<PendingMessage[]>([]);
  // The message the next one replies to, in the selected room.
  let replyingTo = $state<{ eventId: string; quote: Quote } | null>(null);
  // The message of this account the draft is a new version of, and the
  // draft from before the edit began, put back when the edit ends.
  let editingMessage = $state<{ eventId: string; savedDraft: string; savedReply: typeof replyingTo } | null>(null);
  // One of this account's messages waiting for "Delete" to be confirmed,
  // and whether the deletion is being sent.
  let confirmingDelete = $state<{ roomId: string; eventId: string } | null>(null);
  // Where the "New messages" line goes in the selected room, fixed when it
  // opens, and the event this room was last marked read at.
  let unreadFrom = $state<string | null>(null);
  let markedRead: string | null = null;
  // Who else is typing in the selected room, and who has read its newest
  // event (ADR-0023).
  let activity = $state<RoomActivity | null>(null);
  // Sends this account's typing notices in the selected room while the
  // setting is on.
  let typingNotifier: TypingNotifier | null = null;
  let deleting = $state(false);
  // Sending in a room that is not encrypted asks once first (ADR-0005); the
  // core also refuses until the consent is recorded.
  // `image` is true when the question is about sending the attached image.
  let confirmingPlain = $state<{
    roomId: string;
    body: string | null;
    replyTo: string | null;
    retryId: number | null;
    // A reaction waiting for the consent, if the question is about it.
    reaction?: { eventId: string; key: string } | null;
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
  let draftFailure = $state<string | null>(null);
  let draftReady = $state(false);
  let textSending = $state(false);
  let roomOpenRequest = 0;
  let timelineEpoch = 0;
  /** Restore new-message text and its reply quote without replacing an active event edit. */
  function showDraft(next: RoomDraft, failure: string | null, ready: boolean) {
    draftFailure = failure;
    draftReady = ready;
    if (editingMessage) return;
    draft = next.body;
    replyingTo = next.replyTo === null ? null : {
      eventId: next.replyTo,
      quote: quoteOf(applyRelations(items, status?.account.userId ?? null), next.replyTo)
        ?? { sender: 'Earlier message', text: 'Reply context is not loaded.' }
    };
  }
  const draftKeeper = new DraftKeeper({ load: roomDraft, save: saveRoomDraft }, showDraft);
  /** Keep the editor and room-bound store in sync; edits to an existing event stay separate. */
  function changeDraft(body = draft) {
    draft = body;
    if (!editingMessage) draftKeeper.update({ body, replyTo: replyingTo?.eventId ?? null });
  }
  /** Replace a placeholder reply quote after its event enters the loaded timeline. */
  function refreshDraftQuote() {
    if (!replyingTo) return;
    const quote = quoteOf(applyRelations(items, status?.account.userId ?? null), replyingTo.eventId);
    if (quote) replyingTo = { ...replyingTo, quote };
  }
  /** Explicitly clear the current room draft, including a reply reference. */
  async function discardDraft() {
    if (editingMessage) endEdit();
    draft = '';
    replyingTo = null;
    await draftKeeper.discard();
  }
  /** Stop account switching while text is sending or the current draft cannot be persisted. */
  async function prepareAccountSwitch() {
    if (textSending) return false;
    const generation = sessionGeneration;
    return await draftKeeper.flush() && generation === sessionGeneration;
  }
  let preferences = $state<RoomPreference[]>([]);
  let preferenceRequest = 0;
  let preferenceFailure = $state<string | null>(null);
  let roomTool = $state<'jump' | 'pins' | 'attachments' | null>(null);
  let healthOpen = $state(false);
  let focusedContext = $state(false);
  let pinBusy = $state(false);
  let cancelContext: (() => void) | null = null;
  let searchOpen = $state(false);
  let savedDraftsOpen = $state(false);
  let ignoreOpen = $state(false);
  let reportTarget = $state<{ roomId: string; eventId: string; sender: string } | null>(null);
  // The room-section dialog, the member panel, and a favourite being saved.
  let sectionOpen = $state(false);
  let membersShown = $state(true);
  let favouriteBusy = $state(false);
  let favouriteFailure = $state<string | null>(null);
  // The room-groups dialog, and whether it is saving: favourite changes wait
  // for it, because each of its writes carries the room's favourite value.
  let groupsOpen = $state(false);
  let groupsBusy = $state(false);
  // The public room directory dialog.
  let directoryOpen = $state(false);
  // Changes when trust may have changed, so the member panel reads it again.
  let trustVersion = $state(0);
  // The room list width, changed with the resize handle; kept in memory only.
  let sidebarWidth = $state(SIDEBAR_DEFAULT);
  let focusEvent = $state<string | null>(null);
  let searchLocation = $state<string | null>(null);
  /** Open the manager even when a full draft cannot be saved, so another draft can be removed. */
  async function openSavedDrafts() {
    if (textSending) { error = 'Wait for the message to enter the send queue before managing drafts.'; return; }
    const generation = sessionGeneration;
    // A full draft record can make this save fail. The manager must still
    // open so another room's draft can be discarded without losing this text.
    await draftKeeper.flush();
    if (generation === sessionGeneration) savedDraftsOpen = true;
  }
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
  let newConversationOpen = $state(false);
  let newRoomOpen = $state(false);
  // The room the invite dialog is open for.
  let invitingTo = $state<string | null>(null);
  // The room whose settings are open, and a count of change notifications
  // for the selected room, which the settings read again on.
  let roomSettingsOf = $state<string | null>(null);
  let roomChanges = $state(0);
  // This account's settings, from its encrypted profile; the defaults until
  // they load and while no account is signed in.
  let settings = $state<AccountSettings>(DEFAULT_SETTINGS);
  let settingsFailure = $state<string | null>(null);
  const scale = $derived(textScale(settings));
  // The window width in CSS pixels inside the zoomed interface. The room list
  // and member panel shrink or hide so the room keeps a usable width.
  let windowWidth = $state(typeof window === 'undefined' ? 1024 : window.innerWidth);
  const available = $derived(windowWidth / scale.interface);
  const sidebarMax = $derived(sidebarLimit(available));
  const shownSidebar = $derived(Math.min(sidebarWidth, sidebarMax));
  const membersRoom = $derived(membersFit(available, shownSidebar));
  const settingsKeeper = new SettingsKeeper(
    { load: accountSettings, save: setAccountSettings },
    (next, failure) => {
      settings = next;
      settingsFailure = failure;
    }
  );
  // Direct-message invitations, and the one being answered.
  let invites = $state<InviteList | null>(null);
  let inviteBusy = $state<string | null>(null);
  // A finished or cancelled verification stays on screen until this is its ID.
  let dismissedFlow = $state<string | null>(null);
  // A room-list reply replaces the list only if it is newer than the one
  // shown: an older reply that finishes last cannot bring back a stale list,
  // and a failed newer request leaves the newest successful one in place.
  let roomsRequest = 0;
  let roomsShown = 0;
  // The same for the invitation list.
  let invitesRequest = 0;
  let invitesShown = 0;
  let signingIn = $state(false);
  // The sign-in page's server, mode and username. Kept here: every session
  // change remounts the page, and a refused name must stay to be edited.
  let signInForm = $state<SignInForm>({
    homeserver: 'https://matrix.org',
    mode: 'sign_in',
    username: ''
  });
  // Advanced on every session change. Replies to calls made before the change
  // are discarded, so nothing from a locked session is shown again.
  let sessionGeneration = $state(0);

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
  // A member with no direct-message room is shown in the member panel, which
  // offers to start one, rather than as an error. Only once the core has
  // answered this request: until then the error is the previous request's.
  const memberNeedsRoom = $derived(
    memberAsked !== null &&
      memberAsked.room === selected &&
      !securityBusy &&
      security?.operation === 'idle' &&
      security.error === 'not_direct_room'
      ? memberAsked.user
      : null
  );
  // The member panel is shown, so it can show `memberNeedsRoom` itself.
  const membersVisible = $derived(signedIn && membersShown && membersRoom);
  // Why verifying this room's contact or member failed: refused by the
  // host or core (`securityFailure`), or reported by the core afterwards.
  const contactFailure = $derived(
    contactAsked === null || contactAsked !== selected
      ? null
      : (securityFailure ??
          (!(memberNeedsRoom !== null && membersVisible) &&
          security?.operation === 'idle' &&
          security.error
            ? securityErrorText[security.error]
            : null))
  );
  // Why the core could not start verifying the session the Sessions page
  // asked for (such as `cannot_sign_devices`); a flow that did start has its
  // own screen.
  const deviceFailure = $derived(
    deviceAsked !== null && flow === null && security?.operation === 'idle' && security.error
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
  /** Give the lock shortcut priority, then open the room palette only outside modal flows. */
  function shortcutKey(event: KeyboardEvent) {
    // Lock at once, from anywhere, while the profile is open.
    if (isLockShortcut(event, mac)) {
      event.preventDefault();
      if (status?.store === 'protected') void toggleLock();
      return;
    }
    if (!(mac ? event.metaKey : event.ctrlKey) || event.altKey || event.key.toLowerCase() !== 'k') {
      return;
    }
    // Native modal dialogs keep pointer focus inside. Keep the global room
    // shortcut inside the same boundary, especially during saved-draft removal.
    if (savedDraftsOpen || searchOpen || roomTool || healthOpen || ignoreOpen || reportTarget || sectionOpen || groupsOpen || directoryOpen || roomSettingsOf || invitingTo || newConversationOpen || newRoomOpen) return;
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

  /** Rebuild visible account state while rejecting results from an old session generation. */
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
    if (reply.account.state === 'signed_in') {
      refreshSecurity();
      loadPreferences();
    }
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
    refreshInvites();
  }

  /** Ignore preference replies from an older account session or request. */
  async function loadPreferences() {
    const generation = sessionGeneration;
    const request = ++preferenceRequest;
    try {
      const reply = await roomPreferences();
      if (generation === sessionGeneration && request === preferenceRequest) {
        preferences = reply.rooms;
        preferenceFailure = null;
      }
    } catch {
      if (generation === sessionGeneration && request === preferenceRequest) {
        preferenceFailure = 'The room groups could not be loaded.';
      }
    }
  }

  /** Apply a successful room preference write and invalidate an in-flight list read. */
  function preferenceChanged(preference: RoomPreference) {
    preferenceRequest += 1;
    preferences = [...preferences.filter((entry) => entry.roomId !== preference.roomId),
      ...(preference.favourite || preference.section !== null ? [preference] : [])];
  }

  // Invitations change with the room list. A failure keeps the last list;
  // the next change tries again.
  async function refreshInvites() {
    const generation = sessionGeneration;
    const request = ++invitesRequest;
    try {
      const list = await inviteList();
      if (generation === sessionGeneration && request > invitesShown) {
        invites = list;
        invitesShown = request;
      }
    } catch {
      // Kept as it was.
    }
  }

  async function answerInvite(roomId: string, accept: boolean) {
    if (inviteBusy) return;
    const generation = sessionGeneration;
    inviteBusy = roomId;
    try {
      await (accept ? acceptInvite(roomId) : declineInvite(roomId));
      if (generation !== sessionGeneration) return;
      await refreshRooms();
      if (accept && generation === sessionGeneration) open(roomId);
    } catch (reason) {
      if (generation === sessionGeneration) {
        error =
          String(reason) === 'not_found'
            ? 'That invitation is no longer pending.'
            : describe(reason);
        refreshInvites();
      }
    } finally {
      if (generation === sessionGeneration) inviteBusy = null;
    }
  }

  // Load the account's settings each time an account is signed in, and drop
  // them when none is. A session change (reset) clears the status, so a new
  // session always passes through "not signed in" first.
  $effect(() => {
    if (signedIn) settingsKeeper.load();
    else settingsKeeper.clear();
  });

  // The account's display name for its labels. With none set, or none read,
  // the labels show only the user ID.
  let ownName = $state<string | null>(null);
  const ownNameKeeper = new OwnNameKeeper(displayName, (name) => (ownName = name));
  $effect(() => {
    ownNameKeeper.account((signedIn ? status?.account.userId : null) ?? null);
  });

  // A conversation was started or found: show it in the list and open it.
  async function openConversation(roomId: string) {
    const generation = sessionGeneration;
    await refreshRooms();
    if (generation === sessionGeneration) open(roomId);
  }

  /** Clear session-bound UI state and invalidate pending room-context work. */
  function reset() {
    cancelContext?.(); cancelContext = null;
    sessionGeneration += 1;
    roomOpenRequest += 1;
    draftKeeper.clear();
    draftReady = false;
    preferences = [];
    preferenceFailure = null;
    searchOpen = false;
    roomTool = null; healthOpen = false; focusedContext = false; pinBusy = false;
    savedDraftsOpen = false;
    ignoreOpen = false;
    reportTarget = null;
    sectionOpen = false;
    favouriteBusy = false;
    favouriteFailure = null;
    groupsOpen = false;
    groupsBusy = false;
    directoryOpen = false;
    focusEvent = null;
    searchLocation = null;
    textSending = false;
    // A question about the old session no longer applies.
    confirmingSignOut = false;
    view = 'chat';
    paletteOpen = false;
    newConversationOpen = false;
    newRoomOpen = false;
    invitingTo = null;
    roomSettingsOf = null;
    invites = null;
    inviteBusy = null;
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
    memberAsked = null;
    identityAsked = null;
    deviceAsked = null;
    confirmingPlain = null;
    replyingTo = null;
    editingMessage = null;
    confirmingDelete = null;
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

  // New sessions of this account not yet shown (ADR-0026), and whether the
  // alert's action is running.
  let newFound = $state<NewSessions | null>(null);
  let sessionsBusy = $state(false);
  let sessionsFailure = $state<string | null>(null);

  // Reads the new sessions from the core's store; no network request. Best
  // effort: the next check tries again.
  async function checkSessions() {
    const generation = sessionGeneration;
    try {
      const found = await newSessions();
      if (generation === sessionGeneration && !sessionsBusy) {
        newFound = found.sessions.length > 0 ? found : null;
      }
    } catch {
      // Shown as nothing until the next check.
    }
  }

  // Checked when signed in, on each security change and every minute.
  $effect(() => {
    if (!signedIn) {
      newFound = null;
      return;
    }
    checkSessions();
    const timer = setInterval(checkSessions, 60_000);
    return () => clearInterval(timer);
  });

  async function sessionsKnown() {
    const found = newFound;
    if (!found || sessionsBusy) return;
    sessionsBusy = true;
    sessionsFailure = null;
    try {
      await acknowledgeSessions(found.sessions.map((session) => session.deviceId));
      newFound = null;
    } catch (reason) {
      sessionsFailure = describe(reason);
    } finally {
      sessionsBusy = false;
    }
    checkSessions();
  }

  async function removeNewSession(deviceId: string) {
    if (sessionsBusy) return;
    sessionsBusy = true;
    sessionsFailure = null;
    try {
      const outcome = await removeDevice(deviceId);
      if (outcome !== 'done') sessionsFailure = changeText(outcome);
    } catch (reason) {
      sessionsFailure = describe(reason);
    } finally {
      sessionsBusy = false;
    }
    checkSessions();
  }

  // The account's encryption state changed: fetch it again. Nothing else
  // changes.
  async function refreshSecurity() {
    trustVersion += 1;
    checkSessions();
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

  // `asked` names the room whose contact this action verifies, the room in
  // which it accepts a new identity, or the session it verifies; every other
  // action clears all three, so a later
  // failure is not shown in that room. `explain` turns a refusal into text.
  async function startSecurity(
    start: () => Promise<SecurityStatus>,
    asked:
      | { room: string; kind: 'contact' | 'identity' }
      | { room: string; user: string; kind: 'member' }
      | { device: string; kind: 'device' }
      | null = null,
    explain: (reason: unknown) => string = describe
  ) {
    if (securityBusy) return;
    securityBusy = true;
    securityFailure = null;
    // A member's verification fails in the same place as a contact's.
    contactAsked = asked?.kind === 'contact' || asked?.kind === 'member' ? asked.room : null;
    memberAsked = asked?.kind === 'member' ? { room: asked.room, user: asked.user } : null;
    identityAsked = asked?.kind === 'identity' ? asked.room : null;
    deviceAsked = asked?.kind === 'device' ? asked.device : null;
    const generation = sessionGeneration;
    const request = ++securityRequest;
    try {
      const reply = await start();
      if (generation === sessionGeneration && request > securityShown) {
        security = reply;
        securityShown = request;
      }
    } catch (reason) {
      if (generation === sessionGeneration) {
        // A refused request leaves the previous status: its error is not
        // this member's, so no "Start conversation" offer follows from it.
        memberAsked = null;
        securityFailure = explain(reason);
      }
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
    refreshInvites();
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

  async function startSso(homeserver: string) {
    if (signingIn) return;
    signingIn = true;
    const generation = sessionGeneration;
    try {
      const account = await signInWithOidc(homeserver);
      if (generation === sessionGeneration && status) status = { ...status, account };
    } catch (reason) {
      if (generation === sessionGeneration) error = describe(reason);
    } finally {
      signingIn = false;
    }
  }

  async function startRegister(homeserver: string, username: string) {
    if (signingIn) return;
    signingIn = true;
    const generation = sessionGeneration;
    try {
      const account = await register(homeserver, username);
      if (generation === sessionGeneration && status) status = { ...status, account };
    } catch (reason) {
      if (generation === sessionGeneration) {
        error =
          String(reason) === 'invalid_argument'
            ? "Check the username, and use the homeserver's base address, such as https://matrix.example.org."
            : describe(reason);
      }
    } finally {
      signingIn = false;
    }
  }

  /** Start sign-out without waiting for draft persistence; the core attempts server logout before locking. */
  async function confirmSignOut() {
    if (switching) return;
    confirmingSignOut = false;
    switching = true;
    const generation = sessionGeneration;
    try {
      const store = await draftKeeper.protect(signOut);
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

  /** Lock or unlock the active store and ignore status from a session that changed meanwhile. */
  async function toggleLock() {
    if (!status || switching || confirmingSignOut) return;
    switching = true;
    const generation = sessionGeneration;
    try {
      const store = status.store === 'protected' || status.store === 'locking'
        ? await draftKeeper.protect(lockSession)
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
        const stop = await subscribeRoom(roomId, () => {
          roomChanges += 1;
          refresh(roomId, generation);
        });
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

  /** Flush the old room before changing selection; request and epoch guards reject stale history. */
  async function open(roomId: string) {
    cancelContext?.(); cancelContext = null;
    if (savedDraftsOpen) return;
    const generation = sessionGeneration;
    const request = ++roomOpenRequest;
    if (textSending) { error = 'Wait for the message to enter the send queue before changing rooms.'; return; }
    if (!(await draftKeeper.flush()) || generation !== sessionGeneration || request !== roomOpenRequest) return;
    // An attached image belongs to the room it was picked for.
    if (imageDraft && imageDraft.roomId !== roomId) cancelImage();
    imageError = null;
    if (roomSettingsOf !== roomId) roomSettingsOf = null;
    if (selected !== roomId) sectionOpen = false;
    selected = roomId;
    const contentEpoch = ++timelineEpoch;
    focusEvent = null;
    searchLocation = null;
    roomTool = null; focusedContext = false;
    items = [];
    pending = [];
    contact = null;
    changes = null;
    changesFailed = false;
    confirmingPlain = null;
    replyingTo = null;
    confirmingDelete = null;
    unreadFrom = null;
    markedRead = null;
    activity = null;
    typingNotifier?.stop();
    typingNotifier = null;
    editingMessage = null;
    draftReady = false;
    const restoringDraft = signedIn ? draftKeeper.select(roomId) : Promise.resolve();
    if (!signedIn) draft = '';
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
      if (!current(roomId, generation) || contentEpoch !== timelineEpoch) return;
      items = page.items;
      // The restore may finish before history; replace the fallback quote now.
      refreshDraftQuote();
      nextCursor = page.nextCursor;
      const ownUserId = status?.account.userId ?? null;
      unreadFrom = firstUnread(
        applyRelations(page.items, ownUserId),
        selectedRoom?.unreadCount ?? 0,
        ownUserId
      );
      if (signedIn) {
        loadActivity(roomId, generation);
        loadPending(roomId, generation);
        loadContact(roomId, generation);
        loadChanges(roomId, generation);
      }
    } catch (reason) {
      if (current(roomId, generation)) error = describe(reason);
    } finally {
      await restoringDraft;
      if (current(roomId, generation)) {
        opening = false;
        if (contentEpoch === timelineEpoch) {
          loading = false;
          if (changedAgain) refresh(roomId, generation);
        }
      }
    }
  }

  /** Remove focused content on any room invalidation, without fetching or marking it read. */
  function invalidateFocusedContext() {
    cancelContext?.(); cancelContext = null;
    timelineEpoch += 1;
    items = [];
    nextCursor = null;
    focusEvent = null;
    clearReplyContext();
    roomTool = null;
    focusedContext = true;
    loading = false;
    error = null;
    searchLocation = 'Room changed. Return to latest to refresh.';
  }

  /** Remove received quote text while keeping the user's draft and edit body. */
  function clearReplyContext() {
    if (replyingTo) replyingTo = { eventId: replyingTo.eventId, quote: { sender: 'Earlier message', text: 'Reply context is not loaded.' } };
    if (editingMessage?.savedReply) {
      editingMessage = { ...editingMessage, savedReply: { eventId: editingMessage.savedReply.eventId,
        quote: { sender: 'Earlier message', text: 'Reply context is not loaded.' } } };
    }
  }

  /** Refresh live history; clear a focused view on invalidation and reject stale page results. */
  async function refresh(roomId: string, generation: number) {
    if (!current(roomId, generation)) return;
    if (focusedContext) {
      invalidateFocusedContext();
      return;
    }
    if (opening || refreshing) {
      changedAgain = true;
      return;
    }
    refreshing = true;
    const contentEpoch = timelineEpoch;
    try {
      do {
        changedAgain = false;
        const page = await timelinePage(roomId, null);
        if (!current(roomId, generation) || contentEpoch !== timelineEpoch) return;
        items = mergeNewest(items, page.items);
        if (signedIn) {
          loadActivity(roomId, generation);
          await loadPending(roomId, generation);
        }
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

  /** Send once; clear only the unchanged new-message draft and retain text typed during the send. */
  async function queue(
    roomId: string,
    body: string,
    replyTo: string | null,
    generation: number
  ): Promise<boolean> {
    if (textSending) return false;
    textSending = true;
    try {
      await sendMessage(roomId, body, replyTo);
      if (current(roomId, generation)) {
        if (!editingMessage && draft === body && (replyingTo?.eventId ?? null) === replyTo) {
          draft = '';
          replyingTo = null;
          draftKeeper.update({ body: '', replyTo: null });
          await draftKeeper.flush();
        }
        await loadPending(roomId, generation);
      }
      // The parent owns clearing. New text typed during the request stays.
      return false;
    } catch (reason) {
      if (current(roomId, generation)) {
        error =
          String(reason) === 'limit_exceeded'
            ? 'Too many messages are waiting to send. Wait, or discard some.'
            : describe(reason);
      }
      return false;
    } finally {
      if (generation === sessionGeneration) textSending = false;
    }
  }

  // Deletes the message waiting for confirmation. The deletion arrives as a
  // change of the room, which then shows the message as deleted.
  async function confirmDelete() {
    const asked = confirmingDelete;
    if (!asked || deleting) return;
    const generation = sessionGeneration;
    deleting = true;
    try {
      await deleteMessage(asked.roomId, asked.eventId);
      if (current(asked.roomId, generation)) confirmingDelete = null;
    } catch (reason) {
      if (current(asked.roomId, generation)) {
        confirmingDelete = null;
        error =
          String(reason) === 'rejected'
            ? 'The homeserver did not allow this message to be deleted.'
            : describe(reason);
      }
    } finally {
      deleting = false;
    }
  }

  async function sendReaction(roomId: string, eventId: string, key: string) {
    const generation = sessionGeneration;
    try {
      await react(roomId, eventId, key);
    } catch (reason) {
      if (current(roomId, generation)) {
        error =
          String(reason) === 'rejected'
            ? 'The reaction was not sent. Check that this device is verified and no one in this room changed their identity.'
            : describe(reason);
      }
    }
  }

  // Reacts to a message; in a room that is not encrypted, asks first like a
  // message does.
  function reactTo(eventId: string, key: string) {
    const roomId = selected;
    if (!roomId) return;
    if (selectedRoom?.encryption === 'not_encrypted' && !confirmedRooms.has(roomId)) {
      confirmingPlain = {
        roomId,
        body: null,
        replyTo: null,
        retryId: null,
        image: false,
        reaction: { eventId, key }
      };
      return;
    }
    void sendReaction(roomId, eventId, key);
  }

  // Who is typing and who has read the newest event. Best effort: the next
  // change of the room asks again.
  async function loadActivity(roomId: string, generation: number) {
    try {
      const found = await roomActivity(roomId, newestEvent(items));
      if (current(roomId, generation)) activity = found;
    } catch {
      // Shown as nothing until the next change.
    }
  }

  // Tells the selected room this account is typing, while the setting is on.
  function typed() {
    const roomId = selected;
    if (!roomId || !settings.sendTyping) return;
    typingNotifier ??= new TypingNotifier((typing) => {
      setTyping(roomId, typing).catch(() => {});
    });
    typingNotifier.input();
  }

  /** Mark the newest visible event read, except while the user views focused context. */
  async function readTo(eventId: string) {
    if (focusedContext) return;
    const roomId = selected;
    if (!roomId || eventId === markedRead) return;
    markedRead = eventId;
    try {
      await markRead(roomId, eventId);
    } catch {
      if (markedRead === eventId) markedRead = null;
    }
  }

  // Asks the core to open a link; it shows the address and asks first.
  async function followLink(url: string) {
    try {
      await openLink(url);
    } catch (reason) {
      error =
        String(reason) === 'invalid_argument'
          ? 'Persephone opens only web links (http and https) without a name or password in them.'
          : describe(reason);
    }
  }

  // Takes back one of this account's reactions by deleting its event.
  async function unreact(reactionEventId: string) {
    const roomId = selected;
    if (!roomId) return;
    const generation = sessionGeneration;
    try {
      await deleteMessage(roomId, reactionEventId);
    } catch (reason) {
      if (current(roomId, generation)) error = describe(reason);
    }
  }

  /** Enter edit mode only when the new-message draft is ready and no send is active. */
  function startEdit(eventId: string, body: string) {
    if (!draftReady || textSending) return;
    const savedReply = editingMessage?.savedReply ?? replyingTo;
    draftKeeper.beginEdit();
    replyingTo = null;
    editingMessage = { eventId, savedDraft: editingMessage?.savedDraft ?? draft, savedReply };
    draft = body;
  }

  /** Restore the saved new-message body and reply when editing is cancelled. */
  function endEdit() {
    if (!editingMessage) return;
    draftKeeper.endEdit();
    draft = editingMessage.savedDraft;
    replyingTo = editingMessage.savedReply;
    editingMessage = null;
  }

  /** Attach a reply to loaded context and leave event-edit mode before saving the draft. */
  function replyToMessage(eventId: string) {
    const quote = quoteOf(applyRelations(items, status?.account.userId ?? null), eventId);
    if (!quote) return;
    endEdit();
    replyingTo = { eventId, quote };
    changeDraft();
  }

  // Sends the draft as a new version of the message being edited. In a room
  // that is not encrypted the core asks for consent like any message: the
  // edit then waits with "Send unencrypted".
  /** Restore the displaced new-message draft only if this edit still belongs to the active session. */
  async function sendEdit(roomId: string, eventId: string, body: string): Promise<boolean> {
    const generation = sessionGeneration;
    try {
      await editMessage(roomId, eventId, body);
    } catch (reason) {
      if (current(roomId, generation)) {
        error =
          String(reason) === 'limit_exceeded'
            ? 'Too many messages are waiting to send. Wait, or discard some.'
            : describe(reason);
      }
      return false;
    }
    if (current(roomId, generation)) {
      const saved = editingMessage?.eventId === eventId ? editingMessage.savedDraft : '';
      const savedReply = editingMessage?.eventId === eventId ? editingMessage.savedReply : null;
      draftKeeper.endEdit();
      editingMessage = null;
      draft = saved;
      replyingTo = savedReply;
      await loadPending(roomId, generation);
    }
    // The composer keeps the draft put back above.
    return false;
  }

  async function sendText(body: string): Promise<boolean> {
    const roomId = selected;
    if (!roomId) return false;
    typingNotifier?.stop();
    if (editingMessage) return sendEdit(roomId, editingMessage.eventId, body);
    const replyTo = replyingTo?.eventId ?? null;
    if (selectedRoom?.encryption === 'not_encrypted' && !confirmedRooms.has(roomId)) {
      confirmingPlain = { roomId, body, replyTo, retryId: null, image: false };
      return false;
    }
    return queue(roomId, body, replyTo, sessionGeneration);
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
    if (
      asked.body !== null &&
      (await queue(asked.roomId, asked.body, asked.replyTo, generation))
    ) {
      draftVersion += 1;
    }
    if (asked.retryId !== null) changeMessage(retryMessage, asked.retryId);
    if (asked.reaction) await sendReaction(asked.roomId, asked.reaction.eventId, asked.reaction.key);
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

  // Asks the core for a file to send; the core queues it at once, and the
  // pending list shows it (ADR-0029).
  let attachingFile = $state(false);
  async function attachFile() {
    const roomId = selected;
    if (!roomId || attachingFile) return;
    const generation = sessionGeneration;
    attachingFile = true;
    imageError = null;
    try {
      const queued = await pickFile(roomId);
      if (queued && current(roomId, generation)) await loadPending(roomId, generation);
    } catch (reason) {
      if (current(roomId, generation)) {
        imageError =
          String(reason) === 'limit_exceeded'
            ? 'An image or file is already waiting to send. Wait until it is sent, or discard it.'
            : String(reason) === 'rejected'
              ? 'That file cannot be sent: it is empty, larger than 20 MB, or not a plain file.'
              : 'The file could not be attached. Files work on Windows only for now.';
      }
    } finally {
      if (generation === sessionGeneration) attachingFile = false;
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
      confirmingPlain = { roomId: draft.roomId, body: null, replyTo: null, retryId: null, image: true };
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

  // The user left the room from its settings: nothing of it stays shown.
  /** Clear room-bound UI only after the homeserver accepted the leave operation. */
  function leftRoom() {
    cancelContext?.(); cancelContext = null;
    roomOpenRequest += 1; timelineEpoch += 1;
    roomTool = null; focusedContext = false; loading = false;
    // The Leave gate saved the new-message draft while the room was joined.
    // Release only the UI's room-bound copy after the homeserver accepted Leave.
    editingMessage = null;
    draftKeeper.clear();
    draftReady = false;
    roomSettingsOf = null;
    sectionOpen = false;
    if (imageDraft) cancelImage();
    selected = null;
    items = [];
    pending = [];
    contact = null;
    changes = null;
    nextCursor = null;
    error = null;
    refreshRooms();
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

  /** Prepend one bounded history page if the selected room and content epoch still match. */
  async function older() {
    if (!selected || !nextCursor) return;
    const roomId = selected;
    const generation = sessionGeneration;
    const contentEpoch = timelineEpoch;
    loading = true;
    try {
      const page = await timelinePage(roomId, nextCursor);
      if (!current(roomId, generation) || contentEpoch !== timelineEpoch) return;
      items = [...page.items, ...items];
      nextCursor = page.nextCursor;
    } catch (reason) {
      if (current(roomId, generation) && contentEpoch === timelineEpoch) error = describe(reason);
    } finally {
      if (current(roomId, generation) && contentEpoch === timelineEpoch) loading = false;
    }
  }

  /** Load bounded timeline context for a cached hit, then focus it only if the room request is current. */
  async function locateSearchResult(hit: LocalSearchHit) {
    searchOpen = false;
    const generation = sessionGeneration;
    await open(hit.roomId);
    if (!current(hit.roomId, generation)) return;
    const request = roomOpenRequest;
    const contentEpoch = timelineEpoch;
    /** Reject context after a session, room-open request or timeline filter change. */
    function searchContextCurrent() {
      return current(hit.roomId, generation) && request === roomOpenRequest && contentEpoch === timelineEpoch;
    }
    searchLocation = 'Looking for the message in bounded history pages…';
    loading = true;
    try {
      const context = await findSearchContext(hit.roomId, hit.eventId, items, nextCursor,
        timelinePage, searchContextCurrent);
      if (!context) return;
      items = context.items;
      nextCursor = context.cursor;
      await tick();
      if (!current(hit.roomId, generation) || request !== roomOpenRequest || contentEpoch !== timelineEpoch) return;
      focusEvent = context.found ? hit.eventId : null;
      searchLocation = context.found
        ? 'The matching message is selected in the timeline.'
        : 'This cached result is outside the available bounded timeline context. The message could not be located.';
    } catch {
      if (current(hit.roomId, generation) && request === roomOpenRequest && contentEpoch === timelineEpoch) searchLocation = 'The cached result context could not be loaded.';
    } finally {
      if (current(hit.roomId, generation) && request === roomOpenRequest && contentEpoch === timelineEpoch) loading = false;
    }
  }

  /** Drop the old visible timeline immediately, then reload it with the changed ignore filter. */
  async function ignoredChanged() {
    cancelContext?.(); cancelContext = null;
    const roomId = selected;
    if (!roomId) return;
    const generation = sessionGeneration;
    // Drop old content before the filtered page arrives. Draft state stays.
    const contentEpoch = ++timelineEpoch;
    items = [];
    focusEvent = null;
    searchLocation = null;
    roomTool = null; focusedContext = false;
    clearReplyContext();
    nextCursor = null;
    loading = true;
    try {
      const page = await timelinePage(roomId, null);
      if (current(roomId, generation) && contentEpoch === timelineEpoch) { items = page.items; nextCursor = page.nextCursor; refreshDraftQuote(); }
    } catch {
      if (current(roomId, generation) && contentEpoch === timelineEpoch) error = 'The filtered timeline could not be loaded. Try opening this room again.';
    } finally {
      if (current(roomId, generation) && contentEpoch === timelineEpoch) loading = false;
    }
  }
  /** Apply server context only while room, session, request and epoch match. */
  async function locateRoomContext(fetch: (roomId: string) => Promise<FocusedTimeline>) {
    if (!selected || loading) throw new Error('The timeline is busy.');
    const roomId = selected;
    const generation = sessionGeneration;
    const request = roomOpenRequest;
    const epoch = ++timelineEpoch;
    const previousContext = focusedContext;
    focusedContext = true;
    loading = true;
    const valid = () => current(roomId, generation) && request === roomOpenRequest && epoch === timelineEpoch;
    cancelContext = () => {
      if (valid()) { timelineEpoch += 1; loading = false; focusedContext = previousContext; }
    };
    try {
      const context = await fetch(roomId);
      if (!valid()) return;
      items = context.page.items;
      nextCursor = context.page.nextCursor;
      focusedContext = true;
      focusEvent = null;
      refreshDraftQuote();
      cancelContext = null;
      roomTool = null;
      searchLocation = 'Message context is shown. Newer messages can be outside this view.';
      await tick();
      if (valid()) focusEvent = context.eventId;
    } catch (reason) { if (valid()) { error = describe(reason); focusedContext = previousContext; } throw reason; }
    finally { if (valid()) { loading = false; cancelContext = null; } }
  }

  /** Toggle a room in Favourites, keeping its section; ignore a reply from an older session. */
  async function toggleFavourite(roomId: string) {
    if (favouriteBusy || groupsBusy) return;
    const generation = sessionGeneration;
    const saved = preferences.find((entry) => entry.roomId === roomId);
    const preference = { roomId, favourite: !(saved?.favourite ?? false), section: saved?.section ?? null };
    favouriteBusy = true;
    favouriteFailure = null;
    try {
      await setRoomPreference(preference);
      if (generation === sessionGeneration) preferenceChanged(preference);
    } catch {
      if (generation === sessionGeneration) {
        favouriteFailure = 'The favourite could not be saved. This profile may already have the most room groups it can keep.';
      }
    } finally {
      if (generation === sessionGeneration) favouriteBusy = false;
    }
  }

  /**
   * Verify a member of the selected room. The core checks the membership and
   * finds the direct-message room with them itself; with none it reports
   * `not_direct_room`, and the member panel offers to start one. Other
   * failures show in the selected room.
   */
  function verifyMember(userId: string) {
    const roomId = selected;
    if (!roomId) return;
    void startSecurity(() => verifyRoomMember(roomId, userId), { room: roomId, user: userId, kind: 'member' });
  }

  /** Open or start a direct conversation with a member; the result is the failure text, if any. */
  async function messageMember(userId: string): Promise<string | null> {
    const generation = sessionGeneration;
    try {
      const reply = await startConversation(userId);
      if (generation !== sessionGeneration) return null;
      if (reply.kind === 'existing' || reply.kind === 'created') {
        await openConversation(reply.roomId);
        return null;
      }
      return reply.kind === 'no_identity'
        ? 'This person has not set up encryption, so there is nothing to verify.'
        : 'The conversation could not be started.';
    } catch (reason) {
      return String(reason) === 'limit_exceeded'
        ? 'Another conversation is being started. Try again in a moment.'
        : describe(reason);
    }
  }

  // Resizing the room list with the pointer: the pointer that owns the drag,
  // the width and pointer position when it started, and pointer pixels per
  // CSS pixel (the interface zoom). Other pointers cannot move or end it.
  let drag: { pointerId: number; width: number; x: number; ratio: number } | null = null;
  function startResize(event: PointerEvent) {
    if (event.button !== 0) return;
    const handle = event.currentTarget as HTMLElement;
    // A drag in progress keeps its pointer. A drag whose handle went away
    // (the shell was replaced mid-drag) holds no capture and is replaced.
    if (drag && drag.pointerId !== event.pointerId && handle.hasPointerCapture(drag.pointerId)) return;
    const sidebar = handle.previousElementSibling;
    const shown = sidebar instanceof HTMLElement ? sidebar.getBoundingClientRect().width : shownSidebar;
    drag = { pointerId: event.pointerId, width: shownSidebar, x: event.clientX, ratio: shown / shownSidebar };
    handle.setPointerCapture(event.pointerId);
    event.preventDefault();
  }
  function moveResize(event: PointerEvent) {
    if (!drag || event.pointerId !== drag.pointerId) return;
    // No button held: the release was missed, so the drag is over.
    if (event.buttons === 0) {
      drag = null;
      return;
    }
    sidebarWidth = draggedSidebar(drag.width, drag.x, event.clientX, drag.ratio, sidebarMax);
  }
  function endResize(event: PointerEvent) {
    if (!drag || event.pointerId !== drag.pointerId) return;
    drag = null;
    const handle = event.currentTarget as HTMLElement;
    if (handle.hasPointerCapture(event.pointerId)) handle.releasePointerCapture(event.pointerId);
  }
  function keyResize(event: KeyboardEvent) {
    const next = keyedSidebar(shownSidebar, event.key, sidebarMax);
    if (next === null) return;
    event.preventDefault();
    sidebarWidth = next;
  }

  // The main menu: account-wide tools that used to be buttons above the room.
  const mainMenu: MenuItem[] = [
    { label: 'Explore public rooms', onselect: () => (directoryOpen = true) },
    { label: 'Room groups', onselect: () => (groupsOpen = rooms !== null) },
    { label: 'Search cached messages', onselect: () => (searchOpen = true) },
    { label: 'Saved drafts', onselect: () => void openSavedDrafts() },
    { label: 'Ignored users', onselect: () => (ignoreOpen = true) },
    { label: 'Connection health', onselect: () => (healthOpen = true) }
  ];

  /** The selected room's menu: what used to be buttons at the top of the room. */
  function roomMenu(roomId: string, direct: boolean): MenuItem[] {
    return [
      ...(direct ? [] : [{ label: 'Invite people', onselect: () => (invitingTo = roomId) }]),
      { label: 'Jump to date', onselect: () => (roomTool = 'jump') },
      { label: 'Pinned messages', onselect: () => (roomTool = 'pins') },
      { label: 'Attachments', onselect: () => (roomTool = 'attachments') },
      membersRoom
        ? { label: membersShown ? 'Hide members' : 'Show members', onselect: () => (membersShown = !membersShown), separated: true }
        : { label: 'Members need a wider window', onselect: () => {}, disabled: true, separated: true },
      { label: 'Room group', onselect: () => (sectionOpen = true) },
      { label: 'Room settings', onselect: () => (roomSettingsOf = roomId), separated: true }
    ];
  }

  /** Pin through the core; reopen the panel only for the original room request. */
  async function pinMessage(eventId: string) {
    if (!selected || pinBusy) return;
    const roomId = selected; const generation = sessionGeneration; const request = roomOpenRequest;
    const valid = () => current(roomId, generation) && request === roomOpenRequest;
    pinBusy = true;
    try { await setPinned(roomId, eventId, true); if (valid()) roomTool = 'pins'; }
    catch { if (valid()) error = 'The message could not be pinned. Room permission may be required.'; }
    finally { if (generation === sessionGeneration) pinBusy = false; }
  }

</script>


<svelte:window onkeydown={shortcutKey} bind:innerWidth={windowWidth} />

{#snippet brand()}
  <div class="auth-brand">
    <img src={markUrl} alt="" width="32" height="32" />
    Persephone
  </div>
{/snippet}

{#snippet roomMenuButton()}
  {#if selectedRoom}
    <Menu id="room-menu" label="Room menu" items={roomMenu(selectedRoom.roomId, !!selectedRoom.contact)} />
  {/if}
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

<!-- The account's text sizes: the interface zooms as a whole, and message
     text has its own size (style.css). -->
<main style:--ui-scale={scale.interface} style:--chat-scale={scale.chat}>
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
        <SignIn
          account={status.account}
          busy={signingIn}
          bind:form={signInForm}
          onsignin={startSignIn}
          onregister={startRegister}
          onsso={startSso}
        />
        {#if error}
          <p role="alert" class="error">{error}</p>
        {/if}
        {@render lockButton(false, false)}
        <Accounts compact />
      </div>
    </div>
  {:else if shellOpen && view === 'settings'}
    <Settings
      userId={signedIn ? status.account.userId : null}
      {ownName}
      tab={settingsTab}
      ontab={(tab) => (settingsTab = tab)}
      onback={closeSettings}
    >
      {#if draftFailure}
        <div class="draft-status" role="alert"><p class="error">{draftFailure}</p>
          <button type="button" class="secondary" onclick={() => draftKeeper.retry()}>Retry draft</button>
          <button type="button" class="secondary" onclick={discardDraft}>Discard draft</button>
          <button type="button" class="secondary" onclick={closeSettings}>Return to room</button>
        </div>
      {/if}
      {#if settingsTab === 'profile'}
        {#if signedIn && status.account.userId}
          <ProfileSettings
            userId={status.account.userId}
            onsaved={(userId, name) => ownNameKeeper.saved(userId, name)}
          />
        {:else}
          <section class="settings-page" aria-labelledby="profile-heading">
            <h2 id="profile-heading">Profile</h2>
            <p class="lede">No account is signed in. All rooms and messages here are synthetic test data.</p>
          </section>
        {/if}
      {:else if settingsTab === 'account'}
        {#if signedIn}
          <AccountPage native={mac ? 'macOS' : 'Windows'} />
        {:else}
          <section class="settings-page" aria-labelledby="account-heading">
            <h2 id="account-heading">Account</h2>
            <p class="lede">No account is signed in, so there is no password or address to change.</p>
          </section>
        {/if}
      {:else if settingsTab === 'devices'}
        {#if signedIn}
          <Devices
            canVerify={security?.identity === 'verified'}
            busy={securityBusy}
            onverify={(deviceId) =>
              startSecurity(() => verifyDevice(deviceId), { device: deviceId, kind: 'device' })}
          />
          {#if securityFailure}
            <p role="alert" class="error">{securityFailure}</p>
          {:else if deviceFailure}
            <p role="alert" class="error">{deviceFailure}</p>
          {/if}
        {:else}
          <section class="settings-page" aria-labelledby="devices-heading">
            <h2 id="devices-heading">Sessions</h2>
            <p class="lede">No account is signed in, so there are no sessions to list.</p>
          </section>
        {/if}
      {:else if settingsTab === 'preferences'}
        {#if signedIn}
          <Preferences
            {settings}
            failure={settingsFailure}
            {mac}
            onchange={(patch) => settingsKeeper.change(patch)}
          />
        {:else}
          <section class="settings-page" aria-labelledby="preferences-heading">
            <h2 id="preferences-heading">Preferences</h2>
            <p class="lede">No account is signed in. Settings belong to an account, so there are none to change.</p>
          </section>
        {/if}
      {:else if settingsTab === 'accounts'}
        <Accounts beforechange={prepareAccountSwitch} />
        {#if !status || status.store === 'not_configured'}
          <section class="settings-page" aria-labelledby="accounts-demo-heading">
            <h2 id="accounts-demo-heading">Accounts</h2>
            <p class="lede">Synthetic data has no accounts. Start Persephone without --demo to add one.</p>
          </section>
        {/if}
      {:else if settingsTab === 'security'}
        {#if signedIn && security}
          <Security
            {security}
            busy={securityBusy}
            onsetup={() => startSecurity(setUpEncryption)}
            onrecover={() => startSecurity(recoverWithKey)}
            onsetuprecovery={() => startSecurity(setUpRecovery)}
            onverify={() => startSecurity(verifyThisDevice)}
            onchangekey={() => startSecurity(changeRecoveryKey)}
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
          <h2 id="session-heading">This session</h2>
          <div class="panel">
            <p role="status">
              Persephone {status.version} · revision <span class="mono">{status.revision}</span> ·
              {status.milestone} · {storeText[status.store] ??
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
        {preferences}
        userId={signedIn ? status.account.userId : null}
        {ownName}
        {shortcut}
        onselect={open}
        onsearch={() => (paletteOpen = rooms !== null)}
        onsettings={() => openSettings('security')}
        {invites}
        {inviteBusy}
        onnewconversation={signedIn ? () => (newConversationOpen = true) : undefined}
        onnewroom={signedIn ? () => (newRoomOpen = true) : undefined}
        onexplore={signedIn ? () => (directoryOpen = true) : undefined}
        onaccept={(roomId) => answerInvite(roomId, true)}
        ondecline={(roomId) => answerInvite(roomId, false)}
        onfavourite={signedIn ? toggleFavourite : undefined}
        favouriteBusy={favouriteBusy || groupsBusy}
        width={shownSidebar}
      >
        {#snippet menu()}
          {#if signedIn}
            <Menu id="main-menu" label="Main menu" icon="menu" items={mainMenu} />
          {/if}
        {/snippet}
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
      <!-- Drag, or focus and use the arrow keys, to resize the room list. A
           focusable separator is the ARIA window-splitter pattern, an
           interactive widget the linter does not recognise. -->
      <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
      <div
        class="resize-handle"
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize room list"
        aria-valuemin={SIDEBAR_MIN}
        aria-valuemax={sidebarMax}
        aria-valuenow={shownSidebar}
        tabindex="0"
        onpointerdown={startResize}
        onpointermove={moveResize}
        onpointerup={endResize}
        onpointercancel={endResize}
        onkeydown={keyResize}
        ondblclick={() => (sidebarWidth = clampSidebar(SIDEBAR_DEFAULT, sidebarMax))}
      ></div>
      <div class="pane">
        {#if signedIn}
          {#if favouriteFailure}<p class="error pane-error" role="alert">{favouriteFailure}</p>{/if}
          {#if preferenceFailure}<p class="error" role="alert">{preferenceFailure} <button type="button" class="link" onclick={loadPreferences}>Try again</button></p>{/if}
          {#if rooms}<StoredRoomGroups {preferences} joinedRooms={rooms.rooms.map((room) => room.roomId)} onchanged={preferenceChanged} />{/if}
        {/if}
        <!-- Above any room: a new session may be someone else's, so the alert
             does not wait for a room to be opened. -->
        {#if newFound}
          <NewSessionsAlert
            found={newFound}
            busy={sessionsBusy}
            failure={sessionsFailure}
            onknown={sessionsKnown}
            onremove={removeNewSession}
          />
        {/if}
        {#if selectedRoom}
          <div class="room-view">
          <div class="room-main">
          {#if searchLocation || (signedIn && focusedContext)}
            <div class="search-location">
              {#if searchLocation}<p class="note" role="status">{searchLocation}</p>{/if}
              {#if signedIn && focusedContext}<button type="button" class="secondary" onclick={() => selected && open(selected)}>Return to latest</button>{/if}
            </div>
          {/if}
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
            favourite={preferences.some((entry) => entry.roomId === selectedRoom.roomId && entry.favourite)}
            onfavourite={signedIn ? () => toggleFavourite(selectedRoom.roomId) : undefined}
            favouriteBusy={favouriteBusy || groupsBusy}
            menu={signedIn ? roomMenuButton : undefined}
            onreply={sendBlocked === null && draftReady && !textSending
              ? replyToMessage
              : undefined}
            ownUserId={signedIn ? status.account.userId : null}
            onedit={sendBlocked === null ? startEdit : undefined}
            onreport={signedIn ? (eventId, sender) => (reportTarget = { roomId: selectedRoom.roomId, eventId, sender }) : undefined}
            onpin={signedIn && !pinBusy ? pinMessage : undefined}
            {focusEvent}
            ondelete={signedIn
              ? (eventId) => (confirmingDelete = { roomId: selectedRoom.roomId, eventId })
              : undefined}
            onreact={sendBlocked === null ? reactTo : undefined}
            onunreact={signedIn ? unreact : undefined}
            {unreadFrom}
            onread={signedIn && !focusedContext ? readTo : undefined}
            readBy={focusedContext ? [] : (activity?.readBy ?? [])}
            onlink={signedIn ? followLink : undefined}
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
            {#if confirmingDelete}
              <div class="confirm" role="alertdialog" aria-labelledby="delete-question">
                <p id="delete-question">
                  Delete this message for everyone? Other people's devices and servers may already
                  have a copy.
                </p>
                <button type="button" class="danger" disabled={deleting} onclick={confirmDelete}>
                  Delete
                </button>
                <button
                  type="button"
                  class="secondary"
                  disabled={deleting}
                  onclick={() => (confirmingDelete = null)}
                >
                  Keep
                </button>
              </div>
            {/if}
            {#if confirmingPlain}
              <div class="confirm" role="alertdialog" aria-labelledby="plain-question">
                <p id="plain-question">
                  This room is not encrypted: the homeserver and anyone with access to it can read
                  what you send here. {confirmingPlain.image
                    ? 'Send unencrypted messages in this room, starting with this image?'
                    : confirmingPlain.reaction
                      ? 'Send unencrypted messages in this room, starting with this reaction?'
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
              blocked={draftReady ? sendBlocked : 'Loading the saved draft…'}
              roomId={selectedRoom.roomId}
              encryption={selectedRoom.encryption}
              roomName={selectedRoom.displayName ?? selectedRoom.roomId}
              bind:draft
              {draftVersion}
              onsend={sendText}
              onretry={(localId) => changeMessage(retryMessage, localId)}
              ondiscard={(localId) => changeMessage(discardMessage, localId)}
              onallow={(localId) =>
                (confirmingPlain = {
                  roomId: selectedRoom.roomId,
                  body: null,
                  replyTo: null,
                  retryId: localId,
                  image: false
                })}
              {imageDraft}
              {attaching}
              {sendingImage}
              {imageError}
              onattach={attachImage}
              {attachingFile}
              onattachfile={attachFile}
              onsendimage={sendAttached}
              oncancelimage={cancelImage}
              enterSends={settings.enterSends}
              replyingTo={replyingTo?.quote ?? null}
              oncancelreply={() => { replyingTo = null; changeDraft(); }}
              editing={editingMessage !== null}
              oncanceledit={endEdit}
              typing={activity?.typing ?? []}
              typingCut={activity?.truncated ?? false}
              ontyping={signedIn && settings.sendTyping ? typed : undefined}
              onchange={changeDraft}
            />
            {#if draftFailure}
              <div class="draft-status" role="alert"><p class="error">{draftFailure}</p>
                <button type="button" class="secondary" onclick={() => draftKeeper.retry()}>Retry draft</button>
              </div>
            {/if}
            {#if !editingMessage && (draft || replyingTo || draftFailure)}
              <button type="button" class="link discard-draft" disabled={textSending} onclick={discardDraft}>Discard draft</button>
            {/if}
          {/if}
          </div>
          {#if signedIn && membersShown && membersRoom}
            {#key selectedRoom.roomId + ':' + sessionGeneration}
              <MemberPanel
                roomId={selectedRoom.roomId}
                version={roomChanges + trustVersion}
                {contact}
                paused={roomSettingsOf !== null}
                canVerify={security?.identity === 'verified'}
                verifying={securityBusy ||
                  security?.operation !== 'idle' ||
                  (flow !== null && flow.phase !== 'done' && flow.phase !== 'cancelled')}
                asking={memberNeedsRoom}
                onverify={verifyMember}
                oncancelask={() => (memberAsked = null)}
                onmessage={messageMember}
              />
            {/key}
          {/if}
          </div>
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
      {#if healthOpen && signedIn}
        {#key sessionGeneration}<ConnectionHealth onclose={() => (healthOpen = false)} />{/key}
      {/if}
      {#if roomTool && selected && signedIn}
        {#key selected + ':' + sessionGeneration + ':' + roomTool}
          <RoomTools roomId={selected} mode={roomTool} onclose={() => { cancelContext?.(); cancelContext = null; roomTool = null; }}
            ondate={(timestamp) => locateRoomContext((roomId) => jumpToDate(roomId, timestamp))}
            onevent={(eventId) => locateRoomContext((roomId) => eventContext(roomId, eventId))} />
        {/key}
      {/if}
      {#if searchOpen && signedIn}
        <LocalSearch roomId={selected} onselect={locateSearchResult} onclose={() => (searchOpen = false)} />
      {/if}
      {#if savedDraftsOpen && signedIn}
        <SavedDrafts {selected} onclose={() => (savedDraftsOpen = false)} />
      {/if}
      {#if ignoreOpen && signedIn}
        <IgnoreUsers onchanged={ignoredChanged} onclose={() => (ignoreOpen = false)} />
      {/if}
      {#if sectionOpen && signedIn && selected}
        {#key selected}
          <RoomOrganisation roomId={selected} {preferences} favouriteSaving={favouriteBusy || groupsBusy} onchanged={preferenceChanged} onclose={() => (sectionOpen = false)} />
        {/key}
      {/if}
      {#if groupsOpen && signedIn && rooms}
        <RoomGroups rooms={rooms.rooms} {preferences} favouriteSaving={favouriteBusy} onchanged={preferenceChanged}
          onbusy={(busy) => (groupsBusy = busy)} onclose={() => (groupsOpen = false)} />
      {/if}
      {#if directoryOpen && signedIn}
        <PublicRooms homeserver={ownServer(status.account.userId)} onopen={open} onjoined={openConversation}
          onclose={() => (directoryOpen = false)} />
      {/if}
      {#if reportTarget && signedIn}
        <ReportAbuse roomId={reportTarget.roomId} eventId={reportTarget.eventId} sender={reportTarget.sender}
          onclose={() => (reportTarget = null)} onmanageignore={() => { reportTarget = null; ignoreOpen = true; }} />
      {/if}
      {#if newConversationOpen && signedIn}
        <NewConversation onopen={openConversation} onclose={() => (newConversationOpen = false)} />
      {/if}
      {#if newRoomOpen && signedIn}
        <NewRoom onopen={openConversation} onclose={() => (newRoomOpen = false)} />
      {/if}
      {#if invitingTo && signedIn && selectedRoom?.roomId === invitingTo}
        <InvitePerson
          roomId={invitingTo}
          roomName={selectedRoom.displayName ?? selectedRoom.roomId}
          onclose={() => (invitingTo = null)}
        />
      {/if}
      {#if roomSettingsOf && signedIn && selectedRoom?.roomId === roomSettingsOf}
        <RoomSettings
          roomId={roomSettingsOf}
          roomName={selectedRoom.displayName ?? selectedRoom.roomId}
          changes={roomChanges}
          beforeleave={prepareAccountSwitch}
          onleft={leftRoom}
          onclose={() => (roomSettingsOf = null)}
        />
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
        {:else if status.store === 'restart_required' && status.account.state === 'signed_out' && status.account.error === 'session_ended'}
          <p role="alert" class="error">
            The homeserver ended this session. It was signed out from another device, or the
            account was reset or removed.
          </p>
          <p class="auth-note">
            This device's keys were deleted. Restart Persephone to start again with a new profile;
            the rest of this device's local data is deleted as it starts.
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
        {#if status.store !== 'locking' && status.store !== 'unlocking'}
          <Accounts compact />
        {/if}
      </section>
    </div>
  {/if}
</main>
