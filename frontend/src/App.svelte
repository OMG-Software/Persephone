<script lang="ts">
  import { onMount, tick } from 'svelte';
  import AccountPage from './AccountPage.svelte';
  import BrandMark from './BrandMark.svelte';
  import Accounts from './Accounts.svelte';
  import Composer from './Composer.svelte';
  import CallBar from './CallBar.svelte';
  import CallVideo from './CallVideo.svelte';
  import RingPrompt from './RingPrompt.svelte';
  import {
    CAMERA_COMMAND_TEXT,
    SCREEN_COMMAND_TEXT,
    CallRefresher,
    IDLE_CALL,
    JOIN_ERROR_TEXT,
    STARTING_TEXT,
    callButton,
    callReasonNote,
    cameraAvailable,
    keptReasonRoom,
    hasVideo,
    inCall,
    liveRings,
    nextRingEnd,
    shownRings,
    shownSoundLevel,
    videoCallButton,
    CallOperations,
    LatestOnly,
    VOLUME_SAVE_TEXT,
    type ShownRing
  } from './calls';
  import { CALL_WINDOW_UNAVAILABLE_TEXT } from './callWindow';
  import { resettable, storeText, unlockable } from './storeText.ts';
  import RoomTools from './RoomTools.svelte';
  import ConnectionHealth from './ConnectionHealth.svelte';
  import HealthDetails from './HealthDetails.svelte';
  import DiagnosticsSettings from './DiagnosticsSettings.svelte';
  import { callCamera, callDeafen, callDeclineRing, callDevices, callScreen, openCallWindow, callJoin, callLeave, callMute, callReadiness, callRings, callStatus, callVolume, changeRoom, roomMembers, jumpToDate, eventContext, setPinned, setRoomPreference, startConversation } from './bridge';
  import { LEAVE_NOT_DONE, LEAVE_QUESTION, LEAVING_TEXT, leaveAfterDraftFlush } from './roomLeave';
  import type { FocusedTimeline, ImagePick } from '../../crates/contracts/generated/contracts';
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
  import IconAction from './IconAction.svelte';
  import Menu, { type MenuItem } from './Menu.svelte';
  import { EXIT_DRAFT_TEXT, TRAY_UNAVAILABLE_TEXT, exitWhenSaved, mainMenuItems, roomMenuItems } from './menu.ts';
  import MemberPanel from './MemberPanel.svelte';
  import {
    MEMBERS,
    MEMBERS_DEFAULT,
    MEMBERS_MIN,
    SIDEBAR_DEFAULT,
    SIDEBAR_MIN,
    clampSidebar,
    clampWidth,
    draggedSidebar,
    draggedWidth,
    keyedSidebar,
    keyedWidth,
    membersFit,
    membersLimit,
    sidebarLimit
  } from './layout';
  import IdentityChanges from './IdentityChanges.svelte';
  import InvitePerson from './InvitePerson.svelte';
  import JumpPalette from './JumpPalette.svelte';
  import NewConversation from './NewConversation.svelte';
  import NewRoom from './NewRoom.svelte';
  import Preferences from './Preferences.svelte';
  import CallDevices from './CallDevices.svelte';
  import CaptureProtection from './CaptureProtection.svelte';
  import ProfileSettings from './ProfileSettings.svelte';
  import RoomSettings from './RoomSettings.svelte';
  import Security from './Security.svelte';
  import Settings, { type SettingsTab } from './Settings.svelte';
  import Sidebar from './Sidebar.svelte';
  import SignIn, { type SignInForm } from './SignIn.svelte';
  import Timeline from './Timeline.svelte';
  import NewSessionsAlert from './NewSessions.svelte';
  import LoadedCode from './LoadedCode.svelte';
  import { acknowledgeIds, noticeText, platformOf, unreviewed } from './modules';
  import type { DisplaySession } from './bridge';
  import { changeText } from './accountChanges';
  import TipButton from './TipButton.svelte';
  import Verification from './Verification.svelte';
  import { LOCK_HELP, RESET_HELP } from './help';
  import { isolate, withNames } from './people';
  import {
    DEFAULT_SETTINGS,
    SettingsKeeper,
    isLockShortcut,
    lockOffered,
    textScale,
    type SettingsPatch
  } from './preferences';
  import { applyTheme } from './themes.ts';
  import { presenceNotice } from './presence.ts';
  import { OwnNameKeeper } from './ownName';
  import { PASTE_UNSUPPORTED, attachErrorText, sendImageErrorText, type ImageDraft, type PastedImage } from './image';
  import { RoomPictures } from './roomPictures';
  import { securityErrorText } from './securityText';
  import {
    applyRelations,
    firstUnread,
    mergeNewest,
    newestEvent,
    quoteOf,
    replaceUndecryptable,
    undecryptableToRefetch,
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
    pasteImage,
    sendImage,
    exitApp,
    minimiseToTray,
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
    thirdPartyModules,
    acknowledgeModules,
    displaySession,
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
    setProfilePassphrase,
    startNewBackup,
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
    loadRoomPicture,
    imageBand,
    releaseImage,
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
    readEventsAgain,
    timelinePage,
    resetProfile,
    unlockSession,
    watchSession,
    setAccountSettings,
    type AccountSettings,
    type AppStatus,
    type CallReadiness,
    type CallStatus,
    type ContactStatus,
    type InviteList,
    type RoomIdentityChanges,
    type PendingMessage,
    type RoomActivity,
    type NewSessions,
    type ThirdPartyModules,
    type RoomList as Rooms,
    type SecurityStatus,
    type TimelineItem
  } from './bridge';

  let status = $state<AppStatus | null>(null);
  let security = $state<SecurityStatus | null>(null);
  // Whether the profile has a passphrase, null until the core says. It is a
  // property of the profile, so it survives a session change; Lock and the idle
  // lock show only while it is true (ADR-0050).
  let passphraseOn = $state<boolean | null>(null);
  let securityBusy = $state(false);
  // Why the last security action failed. Kept apart from `error`, so a later
  // success clears it without hiding a room's error.
  let securityFailure = $state<string | null>(null);
  // A security reply replaces the shown one only if it is newer, as for rooms.
  let securityRequest = 0;
  let securityShown = 0;
  // Orders the account status reads after an `account` event.
  let accountRequest = 0;
  let accountShown = 0;
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
  // The room waiting for "Leave room" to be confirmed from the room menu,
  // and whether the leave is being sent.
  let confirmingLeave = $state<string | null>(null);
  let leavingRoom = $state(false);
  let leaveQuestion = $state<HTMLElement | null>(null);
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
  // The member panel width, changed with its own handle; in memory only.
  let membersWidth = $state(MEMBERS_DEFAULT);
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
  // Room pictures (ADR-0041): loaded on request, or for every listed room
  // when the account's setting says so; dropped with the session.
  const pictures = new RoomPictures({ load: loadRoomPicture, band: imageBand, release: releaseImage });
  let selected = $state<string | null>(null);
  let items = $state<TimelineItem[]>([]);
  // The display names the open room's pages listed, by user ID. A name can
  // differ between rooms, so it is forgotten when another room opens.
  let senderNames = $state<ReadonlyMap<string, string>>(new Map());
  let nextCursor = $state<string | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let switching = $state(false);
  // Sign-out deletes this device's keys, so it asks once more first.
  let confirmingSignOut = $state(false);
  // The core refused the sign-out: this device's room keys are not all in a
  // key backup. The question then states the loss and offers recovery.
  let signOutLosesKeys = $state(false);
  let keysWarning = $state<HTMLElement | null>(null);
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
  const membersMax = $derived(membersLimit(available, shownSidebar));
  const shownMembers = $derived(Math.min(membersWidth, membersMax));
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
  // Whether a read of older undecryptable events is running (at most one).
  let rereading = false;

  // Fixed store texts and the states Unlock and Reset apply to.

  // Signed in, signed out, or no profile (synthetic data only).
  const profileOpen = $derived(status?.store === 'protected');
  const signedIn = $derived(profileOpen && status?.account.state === 'signed_in');
  // The homeserver refused the presence this account chose, so it is not
  // shared until the setting changes (issue 86). A flag only: no server text.
  const presenceRefused = $derived(
    signedIn && settings.sharePresence && status?.account.presenceRefused === true
  );
  // Keep the pictures in step with the listed rooms; load each by itself only
  // when the account turned that on.
  $effect(() => {
    const listed = rooms?.rooms.map((room) => ({ roomId: room.roomId, pictureId: room.pictureId })) ?? [];
    pictures.sync(listed, signedIn && settings.loadRoomPictures);
  });

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
  // The desktop system, for the Security page text, and the display server
  // on Linux for the capture note (null until the host answers).
  const platform = platformOf(typeof navigator !== 'undefined' ? navigator.platform : '');
  let displayServer = $state<DisplaySession | null>(null);
  const shortcut = mac ? '⌘K' : 'Ctrl K';

  // Ctrl K (⌘K on macOS) opens the jump palette from the room view.
  /** Give the lock shortcut priority, then open the room palette only outside modal flows. */
  function shortcutKey(event: KeyboardEvent) {
    // Lock at once, from anywhere, while the profile is open.
    if (isLockShortcut(event, mac)) {
      event.preventDefault();
      if (lockOffered(status?.store, passphraseOn) && status?.store === 'protected') void toggleLock();
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
    refreshModules();
  }

  function closeSettings() {
    confirmingSignOut = false;
    signOutLosesKeys = false;
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
    if (reply.account.state !== 'signed_in' || reply.store !== 'protected') void refreshPassphrase();
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

  // The account's theme, on the root element so dialogs and the page
  // background follow it. It changes as soon as a choice is made; the
  // keeper puts it back if the core cannot save it.
  $effect(() => {
    applyTheme(document.documentElement, settings.theme);
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
    // Awaited, so a caller sees the room change finished, with any error
    // it set, before it adds its own message.
    if (generation === sessionGeneration) await open(roomId);
  }

  /**
   * Open a room the directory just joined. Say why when it cannot be shown:
   * the room list holds at most 200 rooms and a new room may not have synced
   * yet, and a room change stops when the current draft cannot be saved.
   */
  async function openJoined(roomId: string) {
    const generation = sessionGeneration;
    await openConversation(roomId);
    if (generation !== sessionGeneration) return;
    if (!rooms?.rooms.some((room) => room.roomId === roomId)) {
      error = 'You joined the room, but it is not in the room list yet. The list shows at most 200 rooms, and a new room can take a moment to appear.';
    } else if (selected !== roomId) {
      error ??= 'You joined the room, but the current room could not be left because its draft could not be saved. Select the joined room in the room list.';
    }
  }

  // Audio calls. The core holds the call; this is what it last said. The
  // refresher drops replies from before a session change.
  let callState = $state<CallStatus>(IDLE_CALL);
  let callDismissed = $state(false);
  let callPending = $state(false);
  let callBusy = $state(false);
  let cameraBusy = $state(false);
  let screenBusy = $state(false);
  // The volume the slider is moving to, until the core has the last value.
  let volumeDraft = $state<number | null>(null);
  // Goes up each time the core says a new video picture is waiting.
  let callVideoTicks = $state(0);
  // The own microphone level (ADR-0055, ADR-0059): 0 to 100. It is the
  // call's level while a call is active and a device test's level while no
  // call runs, also right after a call ended. It is 0 while a call joins.
  let callSoundLevel = $state(0);
  function takeSoundLevel(level: number) {
    callSoundLevel = shownSoundLevel(callState.phase, level);
  }
  // Goes up each time a device test changes (ADR-0059).
  let deviceTestTicks = $state(0);
  // The call window opens when a call starts joining (ADR-0060). It opens
  // once per call: closing it during the call keeps it closed until the call
  // bar's "Call window" button.
  let callWindowFor = false;
  $effect(() => {
    const joined = inCall(callState);
    // A window that cannot open leaves the call here; say so, so no window
    // is missed without a reason.
    if (joined && !callWindowFor) void openCallWindow().catch(() => (error = CALL_WINDOW_UNAVAILABLE_TEXT));
    callWindowFor = joined;
  });
  let callJoinText = $state<string | null>(null);
  let cameraText = $state<string | null>(null);
  let screenText = $state<string | null>(null);
  // The newest screen share command; as `cameraRequest`.
  let screenRequest = 0;
  // The newest camera command. Its failure alone shows `cameraText`; a
  // Mute or Leave in between does not hide it. Goes up when the call ends
  // and on a session change, so an old command shows nothing.
  let cameraRequest = 0;
  let readiness = $state<CallReadiness | null>(null);
  let callNames = $state<ReadonlyMap<string, string>>(new Map());
  let readinessRequest = 0;
  let callNamesFor: string | null = null;
  const callOperations = new CallOperations();
  // A camera failure is about the call that is running; it goes when that
  // call ends.
  $effect(() => {
    // Only written, never read here, so a level change does not run this.
    if (callState.phase === 'joining') callSoundLevel = 0;
    if (callState.phase === 'ended' || callState.phase === 'idle') {
      cameraRequest += 1;
      cameraText = null;
      screenRequest += 1;
      screenText = null;
    }
  });
  // A status read during which a call command finished is dropped: that
  // command's reply is newer, and the command asks for a fresh read.
  const callRefresher = new CallRefresher(
    callStatus,
    (next) => {
      callState = next;
      if (next.phase !== 'ended') callDismissed = false;
    },
    () => callOperations.completed
  );

  // Shown when the last volume from the call bar was not saved. The running
  // call may already use it, but the next call would not (ADR-0055).
  let volumeSaveText = $state<string | null>(null);

  // Sends the newest call volume only, one at a time (ADR-0055). A failure
  // goes to `LatestOnly`, which reports only the last value's outcome.
  const volumeSender = new LatestOnly<number>(
    async (percent) => {
      const generation = sessionGeneration;
      const operation = callOperations.begin();
      try {
        const reply = await callVolume(percent);
        if (generation === sessionGeneration && callOperations.isCurrent(operation)) callState = reply;
      } finally {
        if (generation === sessionGeneration) callOperations.complete(operation);
      }
    },
    (failed) => {
      volumeDraft = null;
      volumeSaveText = failed ? VOLUME_SAVE_TEXT : null;
      callRefresher.request();
    }
  );

  function changeVolume(percent: number) {
    volumeDraft = percent;
    volumeSender.set(percent);
  }

  const callRoomName = $derived(
    callState.roomId === null
      ? ''
      : (rooms?.rooms.find((room) => room.roomId === callState.roomId)?.displayName ?? callState.roomId)
  );
  const callView = $derived(
    selected === null ? null : callButton(selected, readiness, callState, callPending)
  );
  const videoView = $derived(
    selected === null ? null : videoCallButton(selected, readiness, callState, callPending)
  );
  // The room where a click on a call button asked why no call can start.
  // The answer is visible text; the button's own description reaches only
  // screen readers.
  let callReasonFor = $state<string | null>(null);
  const callReasonText = $derived(
    selected === null ? null : callReasonNote(selected, callReasonFor, callView)
  );
  // The click is forgotten once its note goes: a room change, a ready room
  // or a hidden button. A later block then needs a new click.
  $effect(() => {
    callReasonFor = keptReasonRoom(selected, callReasonFor, callView);
  });

  // Ask again whether the shown room can take a call when the room or the
  // call changes. Speaking changes alone do not.
  const readinessKey = $derived(
    `${selected}|${callState.phase}|${callState.roomId}|${callState.participants.length}`
  );
  $effect(() => {
    readinessKey;
    refreshReadiness(selected);
  });

  async function refreshReadiness(roomId: string | null) {
    const generation = sessionGeneration;
    const request = ++readinessRequest;
    if (roomId === null || !signedIn) {
      readiness = null;
      return;
    }
    try {
      const reply = await callReadiness(roomId);
      if (generation === sessionGeneration && request === readinessRequest) readiness = reply;
    } catch {
      if (generation === sessionGeneration && request === readinessRequest) readiness = null;
    }
  }

  // Names for the call bar come from the call room's members.
  $effect(() => {
    const roomId = callState.roomId;
    if (roomId === null || !inCall(callState)) return;
    if (callNamesFor === roomId) return;
    callNamesFor = roomId;
    const generation = sessionGeneration;
    roomMembers(roomId, '')
      .then((reply) => {
        if (generation !== sessionGeneration || callNamesFor !== roomId) return;
        const found = new Map<string, string>();
        for (const member of reply.members) {
          if (member.displayName !== null) found.set(member.userId, member.displayName);
        }
        callNames = found;
      })
      .catch(() => {
        if (callNamesFor === roomId) callNamesFor = null;
      });
  });

  function focusCallControl() {
    tick().then(() => {
      const target =
        document.querySelector<HTMLElement>('.call-dismiss') ??
        document.querySelector<HTMLElement>('.call-start:not([aria-disabled="true"]):not(:disabled)');
      target?.focus();
    });
  }

  async function startCall(video: boolean) {
    if (!selected || callPending) return;
    if (callView?.disabled) {
      // A button that cannot start a call says why, rather than nothing.
      // An earlier join error would hide that answer, so this click ends it.
      callJoinText = null;
      callReasonFor = selected;
      return;
    }
    callReasonFor = null;
    await runJoin(selected, video);
  }

  // Joins the call in `roomId`: from the header's buttons, or by answering a
  // ring. The core runs every join check either way.
  async function runJoin(roomId: string, video: boolean) {
    const generation = sessionGeneration;
    callPending = true;
    callJoinText = null;
    const operation = callOperations.begin();
    try {
      const reply = await callJoin(roomId, video);
      if (generation !== sessionGeneration || !callOperations.isCurrent(operation)) return;
      callJoinText = reply.joinError === null ? null : JOIN_ERROR_TEXT[reply.joinError];
      callState = reply;
      if (reply.phase !== 'ended') callDismissed = false;
    } catch {
      if (generation === sessionGeneration) callJoinText = JOIN_ERROR_TEXT.declined;
    } finally {
      // A command of an old session must not drop the new session's read.
      if (generation === sessionGeneration) {
        callOperations.complete(operation);
        callPending = false;
        callRefresher.request();
      }
    }
  }

  // Incoming call rings (ADR-0052). The core lists them; this page stamps
  // each with its end and drops it when the time passes. Nothing here joins
  // a call except an answer button.
  let rings = $state<ShownRing[]>([]);
  let ringCamera = $state(false);
  let ringBusy = $state<ReadonlySet<string>>(new Set());
  // Goes up when a newer list arrives, so an older read cannot replace it.
  let ringRequest = 0;
  // Goes up when a decline starts; only the newest decline's reply is shown.
  let declineRequest = 0;
  let ringsRunning = false;
  let ringsAgain = false;
  let ringDevicesAsked = false;

  async function refreshRings() {
    if (ringsRunning) {
      ringsAgain = true;
      return;
    }
    ringsRunning = true;
    try {
      do {
        ringsAgain = false;
        const generation = sessionGeneration;
        const request = ringRequest;
        try {
          const reply = await callRings();
          if (generation === sessionGeneration && request === ringRequest) {
            rings = shownRings(reply, Date.now());
          }
        } catch {
          // Keep what is shown; the next change asks again.
        }
      } while (ringsAgain);
    } finally {
      ringsRunning = false;
    }
  }

  function markRingBusy(roomId: string, on: boolean) {
    const next = new Set(ringBusy);
    if (on) next.add(roomId);
    else next.delete(roomId);
    ringBusy = next;
  }

  async function answerRing(roomId: string, video: boolean) {
    if (callPending || ringBusy.has(roomId)) return;
    const generation = sessionGeneration;
    markRingBusy(roomId, true);
    try {
      await runJoin(roomId, video);
    } finally {
      if (generation === sessionGeneration) {
        markRingBusy(roomId, false);
        refreshRings();
      }
    }
  }

  async function declineRing(roomId: string) {
    if (ringBusy.has(roomId)) return;
    const generation = sessionGeneration;
    const request = ++declineRequest;
    markRingBusy(roomId, true);
    try {
      const reply = await callDeclineRing(roomId);
      if (generation === sessionGeneration && request === declineRequest) {
        ringRequest += 1;
        rings = shownRings(reply, Date.now());
      }
    } catch {
      // The next read shows what the core still holds.
    } finally {
      if (generation === sessionGeneration) {
        markRingBusy(roomId, false);
        refreshRings();
      }
    }
  }

  // A ring leaves the page when its time is up, even before the core says so.
  $effect(() => {
    const wait = nextRingEnd(rings, Date.now());
    if (wait === null) return;
    const timer = setTimeout(() => (rings = liveRings(rings, Date.now())), wait + 1);
    return () => clearTimeout(timer);
  });

  // Answering with video is offered only when a camera is connected. The
  // device list is read once each time rings appear, and not during a call.
  $effect(() => {
    if (rings.length === 0) {
      ringDevicesAsked = false;
      ringCamera = false;
      return;
    }
    if (ringDevicesAsked || inCall(callState)) return;
    ringDevicesAsked = true;
    const generation = sessionGeneration;
    callDevices().then(
      (devices) => {
        if (generation === sessionGeneration) ringCamera = cameraAvailable(devices);
      },
      () => {}
    );
  });

  const ringRoomNames = $derived(
    new Map((rooms?.rooms ?? []).map((room) => [room.roomId, room.displayName ?? room.roomId]))
  );

  async function toggleMute() {
    const generation = sessionGeneration;
    callBusy = true;
    const operation = callOperations.begin();
    try {
      const reply = await callMute(!callState.muted);
      if (generation === sessionGeneration && callOperations.isCurrent(operation)) callState = reply;
    } catch {
      // The next call change shows the real state.
    } finally {
      if (generation === sessionGeneration) {
        callOperations.complete(operation);
        callBusy = false;
        callRefresher.request();
      }
    }
  }

  // Starts or stops the screen share. The page sends no source: on a start the
  // core shows its own warning or chooser, and a cancel leaves the share off.
  async function toggleScreen() {
    const generation = sessionGeneration;
    screenBusy = true;
    screenText = null;
    screenRequest += 1;
    const request = screenRequest;
    const operation = callOperations.begin();
    try {
      const reply = await callScreen(callState.screen !== 'on' && callState.screen !== 'starting');
      if (generation === sessionGeneration && callOperations.isCurrent(operation)) callState = reply;
    } catch {
      if (generation === sessionGeneration && request === screenRequest) {
        screenText = SCREEN_COMMAND_TEXT;
      }
    } finally {
      if (generation === sessionGeneration) {
        callOperations.complete(operation);
        screenBusy = false;
        callRefresher.request();
      }
    }
  }

  async function toggleDeafen() {
    const generation = sessionGeneration;
    callBusy = true;
    const operation = callOperations.begin();
    try {
      const reply = await callDeafen(!callState.deafened);
      if (generation === sessionGeneration && callOperations.isCurrent(operation)) callState = reply;
    } catch {
      // The next call change shows the real state.
    } finally {
      if (generation === sessionGeneration) {
        callOperations.complete(operation);
        callBusy = false;
        callRefresher.request();
      }
    }
  }

  async function toggleCamera() {
    const generation = sessionGeneration;
    cameraBusy = true;
    cameraText = null;
    cameraRequest += 1;
    const request = cameraRequest;
    const operation = callOperations.begin();
    try {
      const reply = await callCamera(callState.camera !== 'on' && callState.camera !== 'starting');
      if (generation === sessionGeneration && callOperations.isCurrent(operation)) callState = reply;
    } catch {
      // The command failed before the core answered. The refresh shows the
      // real camera state; this says why it did not change.
      if (generation === sessionGeneration && request === cameraRequest) {
        cameraText = CAMERA_COMMAND_TEXT;
      }
    } finally {
      if (generation === sessionGeneration) {
        callOperations.complete(operation);
        cameraBusy = false;
        callRefresher.request();
      }
    }
  }

  async function leaveCall() {
    const generation = sessionGeneration;
    const operation = callOperations.begin();
    try {
      const reply = await callLeave();
      if (generation === sessionGeneration && callOperations.isCurrent(operation)) callState = reply;
    } catch {
      // The next call change shows the real state.
    } finally {
      if (generation === sessionGeneration) {
        callOperations.complete(operation);
        callRefresher.request();
      }
    }
    focusCallControl();
  }

  function dismissCall() {
    callDismissed = true;
    focusCallControl();
  }

  /** Clear session-bound UI state and invalidate pending room-context work. */
  function reset() {
    callOperations.reset();
    callRefresher.reset();
    readiness = null;
    readinessRequest += 1;
    callPending = false;
    callBusy = false;
    cameraBusy = false;
    screenBusy = false;
    volumeSender.clear();
    volumeDraft = null;
    volumeSaveText = null;
    callJoinText = null;
    cameraText = null;
    cameraRequest += 1;
    screenText = null;
    screenRequest += 1;
    callDismissed = false;
    callNames = new Map();
    callNamesFor = null;
    callRefresher.request();
    rings = [];
    ringRequest += 1;
    ringBusy = new Set();
    ringCamera = false;
    ringDevicesAsked = false;
    refreshRings();
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
    signOutLosesKeys = false;
    view = 'chat';
    paletteOpen = false;
    newConversationOpen = false;
    newRoomOpen = false;
    invitingTo = null;
    roomSettingsOf = null;
    invites = null;
    inviteBusy = null;
    modules = null;
    modulesFailure = null;
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
    confirmingLeave = null;
    // The core dropped every image handle with the session.
    imageDraft = null;
    attaching = false;
    sendingImage = false;
    imageError = null;
    confirmedRooms = new Set();
    selected = null;
    rooms = null;
    pictures.clear();
    items = [];
    senderNames = new Map();
    nextCursor = null;
    error = null;
    // The host already ended the room subscription.
    live = null;
    // Operations from the old session no longer update these.
    loading = false;
    opening = false;
    changedAgain = false;
    load();
    refreshModules();
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

  // The report of code loaded into Persephone. Its reviewed marks belong to
  // the open profile, so a session change clears it until the new report
  // arrives. A failed fetch keeps the last report of the same session; the
  // next event or opening of Settings tries again.
  let modules = $state<ThirdPartyModules | null>(null);
  let modulesBusy = $state(false);
  let modulesFailure = $state<string | null>(null);
  let modulesRequest = 0;
  let modulesShown = 0;
  const modulesNotice = $derived(noticeText(modules));

  async function refreshModules() {
    const generation = sessionGeneration;
    const request = ++modulesRequest;
    try {
      const reply = await thirdPartyModules();
      if (generation === sessionGeneration && request > modulesShown) {
        modules = reply;
        modulesShown = request;
      }
    } catch {
      // The report stays as it was.
    }
  }

  // Marks the unreviewed modules in the report as reviewed.
  async function reviewModules() {
    const ids = acknowledgeIds(unreviewed(modules));
    if (ids.length === 0 || modulesBusy) return;
    const generation = sessionGeneration;
    // Numbered at the start, like a refresh, so a refresh that starts while
    // this waits and carries a newer scan is not dropped.
    const request = ++modulesRequest;
    modulesBusy = true;
    modulesFailure = null;
    try {
      const reply = await acknowledgeModules(ids);
      if (generation === sessionGeneration && request > modulesShown) {
        modules = reply;
        modulesShown = request;
      }
    } catch (reason) {
      if (generation === sessionGeneration) modulesFailure = describe(reason);
    } finally {
      modulesBusy = false;
    }
  }

  // Learn whether the profile has a passphrase while no account is signed in
  // or the profile is closed. The core reports it in every store state.
  async function refreshPassphrase() {
    try {
      passphraseOn = (await securityStatus()).profilePassphrase;
    } catch {
      // Lock stays hidden until a later status says so.
    }
  }

  /**
   * The account's status changed within its session, such as a presence
   * refusal: read it again and take only that flag, clearing nothing. A
   * change of account or state arrives as a session event instead.
   */
  async function refreshAccount() {
    const generation = sessionGeneration;
    const request = ++accountRequest;
    let reply: AppStatus;
    try {
      reply = await appStatus();
    } catch {
      // The next account or session event tries again.
      return;
    }
    if (generation !== sessionGeneration || request <= accountShown || !status) return;
    accountShown = request;
    const same =
      reply.account.state === status.account.state && reply.account.userId === status.account.userId;
    if (same) status = { ...status, account: { ...status.account, presenceRefused: reply.account.presenceRefused } };
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
      if (generation === sessionGeneration) passphraseOn = reply.profilePassphrase;
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
      if (generation === sessionGeneration) passphraseOn = reply.profilePassphrase;
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

  /** Start sign-out without waiting for draft persistence; the core attempts server logout before locking.
   * `discardKeys` is the user's choice to lose room keys that are not in a key backup. */
  async function confirmSignOut(discardKeys: boolean) {
    if (switching) return;
    confirmingSignOut = false;
    signOutLosesKeys = false;
    switching = true;
    const generation = sessionGeneration;
    try {
      const store = await draftKeeper.protect(() => signOut(discardKeys));
      if (generation === sessionGeneration && status) status = { ...status, store };
    } catch (reason) {
      if (generation === sessionGeneration) {
        // The session goes on: save the draft whose autosave `protect` stopped.
        void draftKeeper.flush();
        if (String(reason) === 'keys_not_backed_up') {
          // Nothing changed. Ask again, with the loss stated.
          signOutLosesKeys = true;
          confirmingSignOut = true;
          void focusKeysWarning();
        } else {
          error =
            String(reason) === 'limit_exceeded'
              ? 'Persephone is busy, so signing out did not start. Try again.'
              : describe(reason);
        }
      }
    } finally {
      switching = false;
    }
  }

  // The warning replaces the question that had focus, so it takes focus
  // once it shows.
  async function focusKeysWarning() {
    await tick();
    keysWarning?.focus();
  }

  // Closes the warning and gives focus back to the Sign out button.
  async function keepSignedIn() {
    confirmingSignOut = false;
    signOutLosesKeys = false;
    await tick();
    document.getElementById('sign-out-button')?.focus();
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
      await watchSession(reset, refreshRooms, refreshSecurity, refreshModules, () => callRefresher.request(), () => (callVideoTicks += 1), refreshRings, refreshAccount, takeSoundLevel, () => {}, () => (deviceTestTicks += 1));
    } catch {
      unavailable = true;
      return;
    }
    callRefresher.request();
    refreshRings();
    load();
    refreshModules();
    if (platform === 'linux') {
      displaySession().then(
        (reply) => (displayServer = reply),
        () => {
          // The note says the session is not known.
        }
      );
    }
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
    if (selected !== roomId) senderNames = new Map();
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
    confirmingLeave = null;
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
      senderNames = withNames(senderNames, page, true);
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
        senderNames = withNames(senderNames, page, true);
        // Not awaited: newer pages do not wait for older events.
        void refetchUndecryptable(roomId, generation, contentEpoch, page.items);
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

  /**
   * After a change, read loaded undecryptable items above the newest page
   * again: the newest page shows only its own events decrypted with keys that
   * arrived. The core reads them from the homeserver only when room keys
   * arrived for the room; otherwise it answers at once with no items. Items
   * are replaced in place, so the scroll position is not set. A stale or
   * failed result changes nothing; after a failed read the core keeps its
   * mark, so the next change asks again.
   *
   * It runs beside `refresh`, which does not wait for it. At most one read
   * runs at a time; a change during it does not ask again, and the core keeps
   * any new mark for the next change.
   */
  async function refetchUndecryptable(
    roomId: string,
    generation: number,
    contentEpoch: number,
    newest: TimelineItem[]
  ) {
    if (!signedIn || rereading) return;
    const eventIds = undecryptableToRefetch(items, newest);
    if (eventIds.length === 0) return;
    rereading = true;
    try {
      const page = await readEventsAgain(roomId, eventIds);
      if (!current(roomId, generation) || contentEpoch !== timelineEpoch) return;
      if (page.roomId !== roomId || page.items.length === 0) return;
      items = replaceUndecryptable(items, page.items);
      senderNames = withNames(senderNames, page, false);
    } catch {
      // Best effort: the notices stay as they are until the next change.
    } finally {
      rereading = false;
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

  // Asks before leaving. The menu gives focus back to its button, so the
  // question takes it after it shows.
  async function askLeave(roomId: string) {
    confirmingLeave = roomId;
    await tick();
    leaveQuestion?.focus();
  }

  // Closes the question and gives focus back to the room menu.
  async function stayInRoom() {
    confirmingLeave = null;
    await tick();
    document.getElementById('room-menu-button')?.focus();
  }

  // Leaves the room waiting for confirmation. The unsaved draft is saved
  // first, as before an account switch, and the composer stays disabled
  // until the leave ends, so no later text is lost. The room view clears
  // only after the homeserver accepted the leave, and only when the same
  // room is still selected when this code runs again.
  async function confirmLeave() {
    const roomId = confirmingLeave;
    if (!roomId || leavingRoom) return;
    const generation = sessionGeneration;
    const valid = () => current(roomId, generation);
    leavingRoom = true;
    try {
      const left = await leaveAfterDraftFlush(
        prepareAccountSwitch,
        () => changeRoom(roomId, { kind: 'leave' }),
        valid
      );
      if (!valid()) return;
      confirmingLeave = null;
      if (left) leftRoom();
      else error = LEAVE_NOT_DONE;
    } catch (reason) {
      if (valid()) {
        confirmingLeave = null;
        error = describe(reason);
      }
    } finally {
      leavingRoom = false;
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

  // Makes an image draft for the selected room: from the core's file dialog
  // or from a paste (ADR-0042). Either way the core strips and decodes the
  // image, and the draft waits for Send. A draft that arrives after the room
  // or the session changed is dropped at once.
  async function attachWith(draft: (roomId: string) => Promise<ImagePick>) {
    const roomId = selected;
    if (!roomId || attaching || imageDraft) return;
    const generation = sessionGeneration;
    attaching = true;
    imageError = null;
    try {
      const pick = await draft(roomId);
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

  const attachImage = () => attachWith(pickImage);

  // A paste that holds an image attaches it, unless one is already attached.
  function pasteAttached(pasted: PastedImage) {
    if (!selected || pasted.kind === 'none') return;
    if (pasted.kind === 'unsupported') {
      imageError = PASTE_UNSUPPORTED;
    } else if (attaching || imageDraft) {
      imageError = 'An image is already attached. Send or cancel it, then paste again.';
    } else {
      void attachWith((roomId) => pasteImage(roomId, pasted.file));
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
    senderNames = new Map();
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
      senderNames = withNames(senderNames, page, false);
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
      const named = async (roomId: string, cursor: string | null) => {
        const page = await timelinePage(roomId, cursor);
        if (searchContextCurrent()) senderNames = withNames(senderNames, page, cursor === null);
        return page;
      };
      const context = await findSearchContext(hit.roomId, hit.eventId, items, nextCursor,
        named, searchContextCurrent);
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

  /**
   * Saves a change to the settings. The core leaves out what the timeline
   * switches hide, so a change to them reloads the timeline: a switch turned
   * on shows lines the loaded pages never had.
   */
  async function changeSettings(patch: SettingsPatch) {
    // A change that finishes after the session changed must not reload
    // the new session's timeline.
    const generation = sessionGeneration;
    await settingsKeeper.change(patch);
    if (patch.timelineEvents && generation === sessionGeneration) await reloadTimeline();
  }

  /**
   * Drop the old visible timeline immediately, then reload it with the
   * changed filter: the ignore list or the timeline switches.
   */
  async function reloadTimeline() {
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
      if (current(roomId, generation) && contentEpoch === timelineEpoch) { items = page.items; senderNames = withNames(senderNames, page, true); nextCursor = page.nextCursor; refreshDraftQuote(); }
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
      senderNames = withNames(senderNames, context.page, false);
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

  // Resizing the room list or the member panel with the pointer: which
  // column, the pointer that owns the drag, the width and pointer position
  // when it started, and pointer pixels per CSS pixel (the interface zoom).
  // Other pointers cannot move or end it.
  type Column = 'sidebar' | 'members';
  let drag: { column: Column; pointerId: number; width: number; x: number; ratio: number } | null = null;
  function startResize(event: PointerEvent, column: Column) {
    if (event.button !== 0) return;
    const handle = event.currentTarget as HTMLElement;
    // A drag in progress keeps its pointer. A drag whose handle went away
    // (the shell was replaced mid-drag) holds no capture and is replaced.
    if (drag && drag.pointerId !== event.pointerId && handle.hasPointerCapture(drag.pointerId)) return;
    const width = column === 'sidebar' ? shownSidebar : shownMembers;
    // The column the handle resizes: the room list before it, or the
    // member panel after it.
    const beside = column === 'sidebar' ? handle.previousElementSibling : handle.nextElementSibling;
    const shown = beside instanceof HTMLElement ? beside.getBoundingClientRect().width : width;
    drag = { column, pointerId: event.pointerId, width, x: event.clientX, ratio: shown / width };
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
    if (drag.column === 'sidebar') {
      sidebarWidth = draggedSidebar(drag.width, drag.x, event.clientX, drag.ratio, sidebarMax);
    } else {
      // The member panel's handle is on its left edge: it grows leftwards.
      membersWidth = draggedWidth(MEMBERS, drag.width, drag.x, event.clientX, drag.ratio, membersMax, -1);
    }
  }
  function endResize(event: PointerEvent) {
    if (!drag || event.pointerId !== drag.pointerId) return;
    drag = null;
    const handle = event.currentTarget as HTMLElement;
    if (handle.hasPointerCapture(event.pointerId)) handle.releasePointerCapture(event.pointerId);
  }
  function keyResize(event: KeyboardEvent, column: Column) {
    const next =
      column === 'sidebar'
        ? keyedSidebar(shownSidebar, event.key, sidebarMax)
        : keyedWidth(MEMBERS, shownMembers, event.key, membersMax, 'ArrowLeft');
    if (next === null) return;
    event.preventDefault();
    if (column === 'sidebar') sidebarWidth = next;
    else membersWidth = next;
  }

  // Collapse or open the member panel from its own buttons. The button
  // pressed goes away, so focus moves to the one that undoes it.
  async function showMembers(shown: boolean) {
    membersShown = shown;
    await tick();
    document
      .querySelector<HTMLButtonElement>(shown ? '.member-heading button' : '.members-rail button')
      ?.focus();
  }

  // The main menu: account-wide tools, Settings, Lock and Exit.
  const mainMenu = $derived(
    mainMenuItems(
      {
        signedIn,
        canLock: lockOffered(status?.store, passphraseOn),
        switching,
        confirmingSignOut
      },
      {
        explore: () => (directoryOpen = true),
        groups: () => (groupsOpen = rooms !== null),
        search: () => (searchOpen = true),
        drafts: () => void openSavedDrafts(),
        ignored: () => (ignoreOpen = true),
        health: () => (healthOpen = true),
        settings: () => openSettings('security'),
        lock: () => void toggleLock(),
        // A refusal keeps the windows shown; say why, so it is no lost click.
        tray: () => void minimiseToTray().catch(() => (error = TRAY_UNAVAILABLE_TEXT)),
        exit: () =>
          void exitWhenSaved(() => draftKeeper.flush(), exitApp)
            .then((exited) => {
              if (!exited) error = EXIT_DRAFT_TEXT;
            })
            .catch(() => {})
      }
    )
  );

  /** The selected room's menu: what used to be buttons at the top of the room. */
  function roomMenu(roomId: string, direct: boolean): MenuItem[] {
    return roomMenuItems(
      { direct, membersRoom, membersShown, leaving: leavingRoom },
      {
        invite: () => (invitingTo = roomId),
        jump: () => (roomTool = 'jump'),
        pins: () => (roomTool = 'pins'),
        attachments: () => (roomTool = 'attachments'),
        members: () => (membersShown = !membersShown),
        group: () => (sectionOpen = true),
        settings: () => (roomSettingsOf = roomId),
        leave: () => void askLeave(roomId)
      }
    );
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
    <BrandMark />
    Persephone
  </div>
{/snippet}

{#snippet roomMenuButton()}
  {#if selectedRoom}
    <Menu id="room-menu" label="Room menu" items={roomMenu(selectedRoom.roomId, !!selectedRoom.contact)} />
  {/if}
{/snippet}

{#snippet lockButton()}
  <!-- Not while the sign-out question is open: it would apply to a locked session. -->
  <!-- Callers show it only while the profile has a passphrase (ADR-0050). -->
  <TipButton
    id="lock-help"
    help={LOCK_HELP}
    class="secondary"
    disabled={switching || confirmingSignOut}
    onclick={toggleLock}
  >
    {switching ? 'Working…' : 'Lock'}
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
        {#if lockOffered(status.store, passphraseOn)}
          {@render lockButton()}
        {/if}
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
      <!-- Above every tab, so a failure from the menu or the call bar (the
           call window, the tray, Exit) shows on any settings tab. -->
      {#if error}
        <p role="alert" class="error">{error}</p>
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
            profilePassphrase={passphraseOn === true}
            {presenceRefused}
            onchange={changeSettings}
          />
        {:else}
          <section class="settings-page" aria-labelledby="preferences-heading">
            <h2 id="preferences-heading">Preferences</h2>
            <p class="lede">No account is signed in. Settings belong to an account, so there are none to change.</p>
          </section>
        {/if}
      {:else if settingsTab === 'calls'}
        <section class="settings-page" aria-labelledby="calls-heading">
          <h2 id="calls-heading">Calls</h2>
          {#if signedIn}
            <p class="lede">
              The microphone, speaker and camera that calls use, and the call volume. They are kept
              in this computer's encrypted profile.
            </p>
            <div class="panel">
              <h3>Devices</h3>
              <CallDevices soundLevel={callSoundLevel} testTicks={deviceTestTicks} videoTicks={callVideoTicks} />
            </div>
          {:else}
            <p class="lede">No account is signed in. Call devices belong to an account, so there are none to choose.</p>
          {/if}
        </section>
      {:else if settingsTab === 'accounts'}
        <Accounts beforechange={prepareAccountSwitch} />
        {#if !status || status.store === 'not_configured'}
          <section class="settings-page" aria-labelledby="accounts-demo-heading">
            <h2 id="accounts-demo-heading">Accounts</h2>
            <p class="lede">Synthetic data has no accounts. Start Persephone without --demo to add one.</p>
          </section>
        {/if}
      {:else if settingsTab === 'diagnostics'}
        <section class="settings-page" aria-labelledby="diagnostics-heading">
          <h2 id="diagnostics-heading">Diagnostics</h2>
          <p class="lede">
            For finding faults: the state of the connection to your homeserver, and a log you can turn on
            while you repeat a problem. Call devices have their own tests under Calls.
          </p>
          <div class="panel">
            <h3>Connection health</h3>
            {#if signedIn}
              {#key sessionGeneration}<HealthDetails />{/key}
            {:else}
              <p class="note">No account is signed in, so there is no connection to show.</p>
            {/if}
          </div>
          <div class="panel">
            <h3>Diagnostic log</h3>
            <DiagnosticsSettings />
          </div>
        </section>
      {:else if settingsTab === 'security'}
        <!-- One column for the whole tab, so every card has the page's spacing. -->
        <div class="settings-page">
          {#if signedIn && security}
            <Security
              {security}
              busy={securityBusy}
              onsetup={() => startSecurity(setUpEncryption)}
              onrecover={() => startSecurity(recoverWithKey)}
              onsetuprecovery={() => startSecurity(setUpRecovery)}
              onverify={() => startSecurity(verifyThisDevice)}
              onchangekey={() => startSecurity(changeRecoveryKey)}
              onpassphrase={(on) => startSecurity(() => setProfilePassphrase(on))}
              onnewbackup={() => startSecurity(startNewBackup)}
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
            <p role="alert" class="security-notice error">{securityFailure}</p>
          {/if}
          {#if signedIn}
            <CaptureProtection
              {settings}
              {platform}
              session={displayServer}
              failure={settingsFailure}
              onchange={(patch) => settingsKeeper.change(patch)}
            />
          {/if}
          <LoadedCode
            report={modules}
            {platform}
            busy={modulesBusy}
            failure={modulesFailure}
            onacknowledge={reviewModules}
          />
        </div>
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
          {#if lockOffered(status.store, passphraseOn)}
            <div class="panel">
              <h3>Lock</h3>
              <p>{LOCK_HELP}</p>
              <div class="actions">{@render lockButton()}</div>
            </div>
          {/if}
          {#if signedIn}
            <div class="panel">
              <h3>Sign out</h3>
              {#if confirmingSignOut && signOutLosesKeys}
                <div class="confirm" role="alertdialog" aria-labelledby="sign-out-question">
                  <p id="sign-out-question" tabindex="-1" bind:this={keysWarning}>
                    This device's message keys are not all in a key backup. If you sign out now,
                    you lose the encrypted history this device can read, unless another of your
                    signed-in devices has the keys. Set up recovery first to keep them.
                  </p>
                  <button
                    type="button"
                    disabled={switching}
                    onclick={() => {
                      confirmingSignOut = false;
                      signOutLosesKeys = false;
                      openSettings('security');
                    }}
                  >
                    Set up recovery
                  </button>
                  <button
                    type="button"
                    disabled={switching}
                    class="danger"
                    onclick={() => confirmSignOut(true)}
                  >
                    Sign out and lose history
                  </button>
                  <button
                    type="button"
                    disabled={switching}
                    class="secondary"
                    onclick={keepSignedIn}
                  >
                    Cancel
                  </button>
                </div>
              {:else if confirmingSignOut}
                <div class="confirm" role="alertdialog" aria-labelledby="sign-out-question">
                  <p id="sign-out-question">
                    Sign out and delete this device's keys and local data? Encrypted history this
                    device could read may not be readable again.
                  </p>
                  <button
                    type="button"
                    disabled={switching}
                    class="danger"
                    onclick={() => confirmSignOut(false)}
                  >
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
                    id="sign-out-button"
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
        presence={signedIn ? settings : null}
        onpresence={signedIn ? (patch) => settingsKeeper.change(patch) : undefined}
        presenceFailure={presenceNotice(settingsFailure, presenceRefused)}
        {shortcut}
        onselect={open}
        onsearch={() => (paletteOpen = rooms !== null)}
        {invites}
        {inviteBusy}
        onnewconversation={signedIn ? () => (newConversationOpen = true) : undefined}
        onnewroom={signedIn ? () => (newRoomOpen = true) : undefined}
        onexplore={signedIn ? () => (directoryOpen = true) : undefined}
        onaccept={(roomId) => answerInvite(roomId, true)}
        ondecline={(roomId) => answerInvite(roomId, false)}
        onfavourite={signedIn ? toggleFavourite : undefined}
        favouriteBusy={favouriteBusy || groupsBusy}
        {pictures}
        width={shownSidebar}
      >
        {#snippet menu()}
          <Menu id="main-menu" label="Main menu" icon="menu" items={mainMenu} align="start" />
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
        onpointerdown={(event) => startResize(event, 'sidebar')}
        onpointermove={moveResize}
        onpointerup={endResize}
        onpointercancel={endResize}
        onkeydown={(event) => keyResize(event, 'sidebar')}
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
        {#if signedIn && modulesNotice}
          <div class="module-notice" role="status">
            <p>{modulesNotice}</p>
            <button type="button" class="secondary" onclick={() => openSettings('security')}>Review</button>
            <button type="button" class="secondary" disabled={modulesBusy} onclick={reviewModules}>{unreviewed(modules).length > 1 ? 'Mark all as reviewed' : 'Mark as reviewed'}</button>
          </div>
        {/if}
        <RingPrompt
          {rings}
          roomNames={ringRoomNames}
          camera={ringCamera}
          busy={ringBusy}
          onanswer={answerRing}
          ondecline={declineRing}
        />
        {#if !selected}
          <!-- An answer with no room open shows its progress and failure
               here; with a room open, the room header shows them. -->
          {#if callPending}
            <p class="call-notice note" role="status">{STARTING_TEXT}</p>
          {:else if callJoinText}
            <p class="call-notice error" role="alert">
              <span>{callJoinText}</span>
              <button type="button" class="secondary" onclick={() => (callJoinText = null)}>Dismiss</button>
            </p>
          {/if}
        {/if}
        <CallBar
          status={callState}
          dismissed={callDismissed}
          roomName={callRoomName}
          names={callNames}
          busy={callBusy}
          {cameraBusy}
          {screenBusy}
          volume={volumeDraft ?? callState.volume}
          soundLevel={callSoundLevel}
          onmute={toggleMute}
          ondeafen={toggleDeafen}
          onvolume={changeVolume}
          oncamera={toggleCamera}
          onscreen={toggleScreen}
          onleave={leaveCall}
          ondismiss={dismissCall}
          onwindow={() => void openCallWindow().catch(() => (error = CALL_WINDOW_UNAVAILABLE_TEXT))}
        />
        {#if cameraText}
          <p class="call-notice camera-notice error" role="alert">
            <span>{cameraText}</span>
            <button type="button" class="secondary" onclick={() => (cameraText = null)}>Dismiss</button>
          </p>
        {/if}
        {#if volumeSaveText}
          <p class="call-notice volume-notice error" role="alert">
            <span>{volumeSaveText}</span>
            <button type="button" class="secondary" onclick={() => (volumeSaveText = null)}>Dismiss</button>
          </p>
        {/if}
        {#if screenText}
          <p class="call-notice screen-notice error" role="alert">
            <span>{screenText}</span>
            <button type="button" class="secondary" onclick={() => (screenText = null)}>Dismiss</button>
          </p>
        {/if}
        {#if hasVideo(callState)}
          <CallVideo tiles={callState.video} names={callNames} ticks={callVideoTicks} />
        {/if}
        {#if selectedRoom}
          <div class="room-view" style:--members-width="{shownMembers}px">
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
            onreply={sendBlocked === null && draftReady && !textSending && !leavingRoom
              ? replyToMessage
              : undefined}
            ownUserId={signedIn ? status.account.userId : null}
            onedit={sendBlocked === null && !leavingRoom ? startEdit : undefined}
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
            pictureId={selectedRoom.pictureId}
            {pictures}
            onloadpicture={signedIn && selectedRoom.pictureId !== null
              ? () => selectedRoom.pictureId !== null && pictures.request(selectedRoom.roomId, selectedRoom.pictureId)
              : undefined}
            names={senderNames}
            events={settings.timelineEvents}
          >
            {#snippet headerAction()}
              {#if signedIn && callView && !callView.hidden}
                <button
                  type="button"
                  class="secondary call-start"
                  disabled={callView.disabled && callView.reason === null}
                  aria-disabled={callView.disabled && callView.reason !== null ? 'true' : undefined}
                  aria-describedby={callView.reason !== null ? 'call-reason' : undefined}
                  onclick={() => startCall(false)}
                >
                  <Icon name="phone" />
                  {callView.label}
                </button>
                {#if videoView && !videoView.hidden}
                  <button
                    type="button"
                    class="secondary call-start call-start-video"
                    disabled={videoView.disabled && videoView.reason === null}
                    aria-disabled={videoView.disabled && videoView.reason !== null ? 'true' : undefined}
                    aria-describedby={videoView.reason !== null ? 'call-reason' : undefined}
                    onclick={() => startCall(true)}
                  >
                    <Icon name="video" />
                    {videoView.label}
                  </button>
                {/if}
                {#if callView.reason !== null}
                  <span id="call-reason" class="visually-hidden">{callView.reason}</span>
                {/if}
              {/if}
            {/snippet}
            {#snippet headerNotice()}
              {#if callPending}
                <p class="call-notice note" role="status">{STARTING_TEXT}</p>
              {:else if callJoinText}
                <p class="call-notice error" role="alert">
                  <span>{callJoinText}</span>
                  <button type="button" class="secondary" onclick={() => (callJoinText = null)}>Dismiss</button>
                </p>
              {:else if callReasonText}
                <p class="call-notice note" role="status">
                  <span>{callReasonText}</span>
                  <button type="button" class="secondary" onclick={() => (callReasonFor = null)}>Dismiss</button>
                </p>
              {/if}
            {/snippet}
          </Timeline>
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
            {#if confirmingLeave === selectedRoom.roomId}
              <div class="confirm" role="alertdialog" aria-labelledby="leave-question">
                <p id="leave-question" tabindex="-1" bind:this={leaveQuestion}>{LEAVE_QUESTION}</p>
                <button type="button" class="danger" disabled={leavingRoom} onclick={confirmLeave}>
                  Leave room
                </button>
                <button
                  type="button"
                  class="secondary"
                  disabled={leavingRoom}
                  onclick={stayInRoom}
                >
                  Stay
                </button>
              </div>
            {/if}
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
              blocked={leavingRoom ? LEAVING_TEXT : draftReady ? sendBlocked : 'Loading the saved draft…'}
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
              onpasteimage={signedIn ? pasteAttached : undefined}
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
          {#if signedIn && membersRoom}
            {#if membersShown}
              <!-- Drag, or focus and use the arrow keys, to resize the member
                   list; it grows to the left. The same window-splitter pattern
                   as the room list's handle. -->
              <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
              <div
                class="resize-handle"
                role="separator"
                aria-orientation="vertical"
                aria-label="Resize member list"
                aria-valuemin={MEMBERS_MIN}
                aria-valuemax={membersMax}
                aria-valuenow={shownMembers}
                tabindex="0"
                onpointerdown={(event) => startResize(event, 'members')}
                onpointermove={moveResize}
                onpointerup={endResize}
                onpointercancel={endResize}
                onkeydown={(event) => keyResize(event, 'members')}
                ondblclick={() => (membersWidth = clampWidth(MEMBERS, MEMBERS_DEFAULT, membersMax))}
              ></div>
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
                  oncollapse={() => showMembers(false)}
                />
              {/key}
            {:else}
              <div class="members-rail">
                <IconAction icon="users" label="Show members" tip="start" onclick={() => showMembers(true)} />
              </div>
            {/if}
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
        <IgnoreUsers onchanged={reloadTimeline} onclose={() => (ignoreOpen = false)} />
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
        <PublicRooms homeserver={ownServer(status.account.userId)} onopen={open} onjoined={openJoined}
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
            {@render lockButton()}
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
