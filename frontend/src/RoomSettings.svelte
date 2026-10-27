<script lang="ts">
  import { onMount } from 'svelte';
  import Icon from './Icon.svelte';
  import LevelPicker from './LevelPicker.svelte';
  import { avatarColour, initials, localpart, looksLikeUserId } from './people';
  import {
    HISTORY,
    JOIN_RULES,
    NOTIFICATIONS,
    addresses,
    applyLocally,
    applyToMembers,
    failureText,
    historyText,
    joinRuleText,
    levelGroups,
    levelLabels,
    levelOf,
    looksLikeAlias,
    membershipText,
    notificationsText,
    roleOf
  } from './roomSettings';
  import {
    changeRoom,
    inviteUser,
    roomMembers,
    roomSettings,
    type LevelKey,
    type RoomChange,
    type RoomMember,
    type RoomMembers,
    type RoomSettings
  } from './bridge';

  // Everything that can be configured for one joined room (handoff 1i). The
  // permissions the core sends only enable controls: the homeserver decides.
  // One change runs at a time, as in the core.
  let {
    roomId,
    roomName,
    changes,
    onleft,
    onclose
  }: {
    roomId: string;
    // Untrusted: shown as isolated text.
    roomName: string;
    // Counts change notifications for the room; each one reads the settings
    // again, since the device's copy of the room's state follows the sync.
    changes: number;
    onleft: () => void;
    onclose: () => void;
  } = $props();

  type Tab = 'general' | 'people' | 'notifications' | 'security' | 'advanced';
  const tabs: { id: Tab; label: string }[] = [
    { id: 'general', label: 'General' },
    { id: 'people', label: 'People' },
    { id: 'notifications', label: 'Notifications' },
    { id: 'security', label: 'Security & privacy' },
    { id: 'advanced', label: 'Advanced' }
  ];

  // The core's limits; these are only a first stop.
  const MAX_NAME = 255;
  const MAX_TOPIC = 4096;
  const MAX_ALIAS = 255;
  const MAX_FILTER = 256;

  let tab = $state<Tab>('general');
  let dialog = $state<HTMLDialogElement | null>(null);
  let body = $state<HTMLDivElement | null>(null);
  let settings = $state<RoomSettings | null>(null);
  let loadFailure = $state<string | null>(null);
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let done = $state<string | null>(null);
  // A change that needs a yes first.
  let confirming = $state<{ change: RoomChange; question: string; action: string } | null>(null);

  // Edited text, and the saved values it started from: a later read
  // replaces a field only while it is unedited.
  let name = $state('');
  let topic = $state('');
  let savedName = '';
  let savedTopic = '';
  let newAlias = $state('');

  let members = $state<RoomMembers | null>(null);
  let membersFailure = $state<string | null>(null);
  let filter = $state('');
  let invitee = $state('');
  // Advanced when a change is refused or a question cancelled, so controls
  // drawn from the settings show the settings again, not the user's choice.
  let revision = $state(0);

  // A reply replaces the shown one only if it is newer; a read asked for
  // while one runs follows it.
  let settingsRequest = 0;
  let settingsShown = 0;
  let reading = false;
  let readAgain = false;
  let listing = false;
  let listAgain = false;

  const own = $derived(settings?.ownLevel ?? null);
  const may = $derived(settings?.may);
  const known = $derived(settings ? addresses(settings) : []);

  async function loadSettings() {
    if (reading) {
      readAgain = true;
      return;
    }
    reading = true;
    try {
      do {
        readAgain = false;
        const request = ++settingsRequest;
        try {
          const reply = await roomSettings(roomId);
          if (request <= settingsShown) continue;
          settingsShown = request;
          settings = reply;
          loadFailure = null;
          if (name === savedName) name = reply.name ?? '';
          if (topic === savedTopic) topic = reply.topic ?? '';
          savedName = reply.name ?? '';
          savedTopic = reply.topic ?? '';
        } catch (reason) {
          // A later read keeps what is shown; only the first one says why.
          if (settings === null) loadFailure = failureText(reason);
        }
      } while (readAgain);
    } finally {
      reading = false;
    }
  }

  // Typing in the filter asks once per finished read, with the latest text.
  async function loadMembers() {
    if (listing) {
      listAgain = true;
      return;
    }
    listing = true;
    try {
      do {
        listAgain = false;
        try {
          members = await roomMembers(roomId, filter.trim());
          membersFailure = null;
        } catch (reason) {
          membersFailure = failureText(reason);
        }
      } while (listAgain);
    } finally {
      listing = false;
    }
  }

  onMount(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.showModal();
    loadSettings();
    return () => {
      dialog?.close();
      previous?.focus();
    };
  });

  // The room changed (a sync): read again what is shown.
  let seen: number | null = null;
  $effect(() => {
    const now = changes;
    if (seen !== null && now !== seen) {
      loadSettings();
      if (tab === 'people') loadMembers();
    }
    seen = now;
  });

  function show(next: Tab) {
    tab = next;
    body?.scrollTo({ top: 0 });
    confirming = null;
    failure = null;
    done = null;
    if (next === 'people' && members === null) loadMembers();
  }

  async function apply(change: RoomChange, success = 'Saved.') {
    if (busy) return;
    busy = true;
    failure = null;
    done = null;
    confirming = null;
    try {
      await changeRoom(roomId, change);
      if (change.kind === 'leave') {
        onleft();
        return;
      }
      if (settings) settings = applyLocally(settings, change);
      if (members) members = applyToMembers(members, change);
      if (change.kind === 'name') savedName = name = change.name.trim();
      if (change.kind === 'topic') savedTopic = topic = change.topic.trim();
      done = success;
      // Local addresses are read from the homeserver, so they are current
      // at once; the rest follows the sync.
      if (change.kind === 'add_alias' || change.kind === 'remove_alias') loadSettings();
    } catch (reason) {
      failure = failureText(reason);
      revision += 1;
    } finally {
      busy = false;
    }
  }

  function ask(change: RoomChange, question: string, action: string) {
    failure = null;
    done = null;
    confirming = { change, question, action };
  }

  function dismiss() {
    confirming = null;
    revision += 1;
  }

  function saveName(event: SubmitEvent) {
    event.preventDefault();
    apply({ kind: 'name', name });
  }

  function saveTopic(event: SubmitEvent) {
    event.preventDefault();
    apply({ kind: 'topic', topic });
  }

  function addAlias(event: SubmitEvent) {
    event.preventDefault();
    const alias = newAlias.trim();
    if (!looksLikeAlias(alias)) return;
    apply({ kind: 'add_alias', alias }, 'Address added.').then(() => {
      if (!failure) newAlias = '';
    });
  }

  function setMain(alias: string | null) {
    if (!settings) return;
    apply({
      kind: 'canonical_alias',
      alias,
      altAliases: settings.altAliases.filter((alt) => alt !== alias)
    });
  }

  function publish(alias: string, published: boolean) {
    if (!settings) return;
    const altAliases = published
      ? [...settings.altAliases, alias]
      : settings.altAliases.filter((alt) => alt !== alias);
    apply({ kind: 'canonical_alias', alias: settings.canonicalAlias, altAliases });
  }

  function setLevel(member: RoomMember, level: number) {
    const change: RoomChange = { kind: 'member_level', userId: member.userId, level };
    if (member.own && own !== null && level < own) {
      ask(
        change,
        'Lower your own level? You cannot raise it again yourself; someone with a higher level would have to.',
        'Lower my level'
      );
    } else if (own !== null && level === own && !member.own) {
      ask(
        change,
        'Give this person your own level? You will not be able to lower it again.',
        'Give my level'
      );
    } else {
      apply(change);
    }
  }

  async function invite(event: SubmitEvent) {
    event.preventDefault();
    const userId = invitee.trim();
    if (busy || !looksLikeUserId(userId)) return;
    await admit(userId, 'Invited.');
    if (!failure) invitee = '';
  }

  // Invites someone, or lets in someone asking to join.
  async function admit(userId: string, success: string) {
    if (busy) return;
    busy = true;
    failure = null;
    done = null;
    try {
      const result = await inviteUser(roomId, userId);
      if (result.kind === 'invited') {
        done = success;
        loadMembers();
      } else if (result.kind === 'no_identity') {
        failure =
          'This person has not set up encryption, or the ID does not exist. They could not read this encrypted room.';
      } else {
        failure = 'The homeserver refused. They may already be in the room, or you may not invite here.';
      }
    } catch (reason) {
      failure = failureText(reason);
    } finally {
      busy = false;
    }
  }

  function cancel(event: Event) {
    event.preventDefault();
    if (!busy) onclose();
  }

  const memberName = (member: RoomMember) => member.displayName ?? localpart(member.userId);
</script>

{#snippet levelRow(key: LevelKey)}
  {#if settings}
    <li class="level-row">
      <label for="level-{key}">{levelLabels[key]}</label>
      <LevelPicker
        id="level-{key}"
        label={levelLabels[key]}
        level={levelOf(settings, key)}
        {own}
        disabled={busy || !may?.powerLevels}
        onpick={(level) => apply({ kind: 'level', key, level })}
      />
    </li>
  {/if}
{/snippet}

<dialog
  class="room-settings"
  aria-labelledby="room-settings-heading"
  bind:this={dialog}
  oncancel={cancel}
>
  <div class="room-settings-frame">
    <aside class="room-settings-rail">
      <h2 id="room-settings-heading" class="room-settings-title">
        <Icon name="hash" />
        <bdi>{roomName}</bdi>
        <span class="visually-hidden">settings</span>
      </h2>
      <nav aria-label="Room settings">
        {#each tabs as item (item.id)}
          <button
            type="button"
            class="room-row"
            aria-current={tab === item.id ? 'page' : undefined}
            onclick={() => show(item.id)}
          >
            {item.label}
          </button>
        {/each}
      </nav>
    </aside>
    <div class="room-settings-body" bind:this={body}>
      <div class="room-settings-head">
        <h3>{tabs.find((item) => item.id === tab)?.label}</h3>
        <button
          type="button"
          class="icon-button"
          aria-label="Close room settings"
          disabled={busy}
          onclick={onclose}
        >
          <Icon name="close" />
        </button>
      </div>

      {#key revision}
      {#if !settings}
        {#if loadFailure}
          <p role="alert" class="error">{loadFailure}</p>
        {:else}
          <p role="status" class="note">Loading the room's settings…</p>
        {/if}
      {:else if tab === 'general'}
        <form class="panel" onsubmit={saveName}>
          <label for="room-name">Name</label>
          <input
            id="room-name"
            class="field"
            type="text"
            maxlength={MAX_NAME}
            autocomplete="off"
            dir="auto"
            disabled={busy || !may?.name}
            bind:value={name}
          />
          <div class="actions">
            <button
              type="submit"
              class="primary"
              disabled={busy || !may?.name || name.trim() === (settings.name ?? '')}>Save name</button
            >
          </div>
        </form>
        <form class="panel" onsubmit={saveTopic}>
          <label for="room-topic">Topic</label>
          <textarea
            id="room-topic"
            class="field"
            rows="3"
            maxlength={MAX_TOPIC}
            dir="auto"
            disabled={busy || !may?.topic}
            bind:value={topic}
          ></textarea>
          <div class="actions">
            <button
              type="submit"
              class="primary"
              disabled={busy || !may?.topic || topic.trim() === (settings.topic ?? '')}>Save topic</button
            >
          </div>
        </form>
        <section class="panel" aria-labelledby="addresses-heading">
          <h4 id="addresses-heading">Addresses</h4>
          <p>
            People can find and join the room by a published address. The main address is the one
            shown first.
          </p>
          <label for="main-address">Main address</label>
          <select
            id="main-address"
            class="field"
            disabled={busy || !may?.canonicalAlias}
            value={settings.canonicalAlias ?? ''}
            onchange={(event) => setMain(event.currentTarget.value || null)}
          >
            <option value="">None</option>
            {#each known as address (address.alias)}
              <option value={address.alias}>{address.alias}</option>
            {/each}
          </select>
          {#if known.length > 0}
            <ul class="address-list">
              {#each known as address (address.alias)}
                <li>
                  <bdi class="mono">{address.alias}</bdi>
                  <span class="address-tags">
                    {#if address.main}<span class="badge">Main</span>
                    {:else if address.published}<span class="badge">Published</span>{/if}
                    {#if address.local}<span class="badge">On your server</span>{/if}
                  </span>
                  <span class="actions">
                    {#if !address.main && may?.canonicalAlias}
                      <button
                        type="button"
                        class="secondary"
                        disabled={busy}
                        onclick={() => publish(address.alias, !address.published)}
                      >
                        {address.published ? 'Unpublish' : 'Publish'}
                        <span class="visually-hidden"><bdi>{address.alias}</bdi></span>
                      </button>
                    {/if}
                    {#if address.local}
                      <button
                        type="button"
                        class="danger"
                        disabled={busy}
                        onclick={() =>
                          ask(
                            { kind: 'remove_alias', alias: address.alias },
                            'Delete this address from your homeserver? It will no longer lead to this room.',
                            'Delete address'
                          )}
                      >
                        Delete <span class="visually-hidden"><bdi>{address.alias}</bdi></span>
                      </button>
                    {/if}
                  </span>
                </li>
              {/each}
            </ul>
          {/if}
          {#if settings.localAliases === null}
            <p class="note">Your homeserver did not say which of its addresses lead here.</p>
          {/if}
          <form class="field-row" onsubmit={addAlias}>
            <label for="new-address" class="visually-hidden">New address</label>
            <input
              id="new-address"
              class="field mono"
              type="text"
              placeholder="#name:your.server"
              maxlength={MAX_ALIAS}
              spellcheck="false"
              autocomplete="off"
              disabled={busy}
              bind:value={newAlias}
            />
            <button type="submit" class="secondary" disabled={busy || !looksLikeAlias(newAlias.trim())}
              >Add address</button
            >
          </form>
          <p class="note">New addresses must be on your own homeserver.</p>
        </section>
        <section class="panel" aria-labelledby="leave-heading">
          <h4 id="leave-heading">Leave room</h4>
          <p>
            You will stop receiving messages from this room. To come back, someone must invite you,
            unless anyone can join.
          </p>
          <div class="actions">
            <button
              type="button"
              class="danger"
              disabled={busy}
              onclick={() =>
                ask(
                  { kind: 'leave' },
                  settings?.joinRule === 'public'
                    ? 'Leave this room?'
                    : 'Leave this room? You will need a new invitation to come back.',
                  'Leave room'
                )}>Leave room</button
            >
          </div>
        </section>
      {:else if tab === 'people'}
        <div class="people-tools">
          <label for="member-filter" class="visually-hidden">Filter members</label>
          <input
            id="member-filter"
            class="field"
            type="search"
            placeholder="Filter members"
            maxlength={MAX_FILTER}
            autocomplete="off"
            bind:value={filter}
            oninput={loadMembers}
          />
        </div>
        {#if may?.invite}
          <form class="field-row" onsubmit={invite}>
            <label for="invite-id" class="visually-hidden">Matrix ID to invite</label>
            <input
              id="invite-id"
              class="field mono"
              type="text"
              placeholder="@name:example.org"
              maxlength="255"
              spellcheck="false"
              autocomplete="off"
              disabled={busy}
              bind:value={invitee}
            />
            <button type="submit" class="primary" disabled={busy || !looksLikeUserId(invitee.trim())}>
              <Icon name="plus" />Invite
            </button>
          </form>
        {/if}
        {#if membersFailure}
          <p role="alert" class="error">{membersFailure}</p>
        {:else if !members}
          <p role="status" class="note">Loading members…</p>
        {:else}
          <p class="note" role="status">
            {members.members.length === 0
              ? 'No members match.'
              : members.truncated
                ? `Showing the first ${members.members.length} members. Filter to find others.`
                : `${members.members.length} listed.`}
          </p>
          <ul class="member-list">
            {#each members.members as member (member.userId)}
              <li class="member-row">
                <span class="avatar {avatarColour(member.userId)}" aria-hidden="true"
                  >{initials(member.userId)}</span
                >
                <div class="member-text">
                  <span class="member-name">
                    <bdi>{memberName(member)}</bdi>
                    {#if member.own}<span class="badge">You</span>{/if}
                    {#if membershipText[member.membership]}
                      <span class="badge">{membershipText[member.membership]}</span>
                    {/if}
                  </span>
                  <bdi class="member-id">{member.userId}</bdi>
                </div>
                {#if member.canChangeLevel && member.level !== null}
                  <LevelPicker
                    id="member-level-{member.userId}"
                    label={`Role of ${memberName(member)}`}
                    level={member.level}
                    {own}
                    disabled={busy}
                    onpick={(level) => setLevel(member, level)}
                  />
                {:else}
                  <span class="member-role">{roleOf(member.level)}</span>
                {/if}
                <span class="actions member-actions">
                  {#if member.membership === 'knock' && may?.invite}
                    <button
                      type="button"
                      class="secondary"
                      disabled={busy}
                      onclick={() => admit(member.userId, 'Let in.')}
                    >
                      Let in <span class="visually-hidden"><bdi>{memberName(member)}</bdi></span>
                    </button>
                  {/if}
                  {#if member.canKick}
                    <button
                      type="button"
                      class="secondary"
                      disabled={busy}
                      onclick={() =>
                        ask(
                          { kind: 'kick', userId: member.userId },
                          member.membership === 'invite'
                            ? 'Withdraw this invitation?'
                            : member.membership === 'knock'
                              ? 'Turn down this request to join?'
                              : 'Remove this person from the room? They can join again if the room lets them.',
                          member.membership === 'invite'
                            ? 'Withdraw'
                            : member.membership === 'knock'
                              ? 'Turn down'
                              : 'Remove'
                        )}
                    >
                      {member.membership === 'invite'
                        ? 'Withdraw'
                        : member.membership === 'knock'
                          ? 'Turn down'
                          : 'Remove'}
                      <span class="visually-hidden"><bdi>{memberName(member)}</bdi></span>
                    </button>
                  {/if}
                  {#if member.canBan}
                    <button
                      type="button"
                      class="danger"
                      disabled={busy}
                      onclick={() =>
                        ask(
                          { kind: 'ban', userId: member.userId },
                          'Ban this person? They are removed and cannot join again until unbanned.',
                          'Ban'
                        )}
                    >
                      Ban <span class="visually-hidden"><bdi>{memberName(member)}</bdi></span>
                    </button>
                  {/if}
                  {#if member.canUnban}
                    <button
                      type="button"
                      class="secondary"
                      disabled={busy}
                      onclick={() => apply({ kind: 'unban', userId: member.userId }, 'Unbanned.')}
                    >
                      Unban <span class="visually-hidden"><bdi>{memberName(member)}</bdi></span>
                    </button>
                  {/if}
                </span>
              </li>
            {/each}
          </ul>
        {/if}
      {:else if tab === 'notifications'}
        <fieldset class="panel">
          <legend>Notify me about</legend>
          {#each NOTIFICATIONS as mode (mode)}
            <label class="switch-row">
              <input
                type="radio"
                name="room-notifications"
                value={mode}
                checked={settings.notifications === mode}
                disabled={busy}
                onchange={() => apply({ kind: 'notifications', mode })}
              />
              <span>{notificationsText[mode]}</span>
            </label>
          {/each}
          <p class="note">
            This is kept on your homeserver and applies to your other devices. Persephone does not show
            notifications yet.
          </p>
        </fieldset>
      {:else if tab === 'security'}
        <section class="panel" aria-labelledby="encryption-heading">
          <h4 id="encryption-heading">Encryption</h4>
          {#if settings.encryption === 'encrypted'}
            <p>
              Messages here are end-to-end encrypted. Only members' verified devices hold the keys.
              Encryption cannot be turned off.
            </p>
          {:else}
            <p>
              {settings.encryption === 'not_encrypted'
                ? 'Messages here are not encrypted: the homeserver and anyone who joins can read them.'
                : 'Persephone does not know yet whether this room is encrypted.'}
            </p>
            {#if may?.encryption}
              <div class="actions">
                <button
                  type="button"
                  class="primary"
                  disabled={busy}
                  onclick={() =>
                    ask(
                      { kind: 'encryption' },
                      'Turn on encryption? It cannot be turned off again. Bridges, bots and clients that cannot encrypt will stop working here.',
                      'Turn on encryption'
                    )}>Turn on encryption</button
                >
              </div>
            {/if}
          {/if}
        </section>
        <fieldset class="panel">
          <legend>Who can join</legend>
          {#each JOIN_RULES as rule (rule)}
            <label class="switch-row">
              <input
                type="radio"
                name="join-rule"
                value={rule}
                checked={settings.joinRule === rule ||
                  (rule === 'invite' && settings.joinRule === 'private')}
                disabled={busy || !may?.joinRules}
                onchange={() =>
                  rule === 'public'
                    ? ask(
                        { kind: 'join_rule', rule },
                        settings?.encryption === 'encrypted'
                          ? 'Let anyone who finds the room join? Everyone who joins can read new messages.'
                          : 'Let anyone who finds the room join? Everyone who joins can read new messages, and the room is not encrypted.',
                        'Let anyone join'
                      )
                    : apply({ kind: 'join_rule', rule })}
              />
              <span>{joinRuleText[rule]}</span>
            </label>
          {/each}
          {#if !JOIN_RULES.includes(settings.joinRule) && settings.joinRule !== 'private'}
            <p class="note">Now: {joinRuleText[settings.joinRule]}.</p>
          {/if}
        </fieldset>
        <section class="panel" aria-labelledby="visibility-heading">
          <h4 id="visibility-heading">Visibility</h4>
          <label class="switch-row">
            <input
              type="checkbox"
              role="switch"
              checked={settings.published === true}
              disabled={busy}
              onchange={(event) =>
                apply({ kind: 'published', published: event.currentTarget.checked })}
            />
            <span>List in your homeserver's public room directory</span>
          </label>
          {#if settings.published === null}
            <p class="note">Your homeserver did not say whether the room is listed.</p>
          {/if}
          <label class="switch-row">
            <input
              type="checkbox"
              role="switch"
              checked={settings.guestAccess}
              disabled={busy || !may?.guestAccess}
              onchange={(event) =>
                apply({ kind: 'guest_access', allowed: event.currentTarget.checked })}
            />
            <span>Let guests join (accounts without a password, where the homeserver allows them)</span>
          </label>
        </section>
        <fieldset class="panel">
          <legend>Who can read the history</legend>
          {#each HISTORY as visibility (visibility)}
            <label class="switch-row">
              <input
                type="radio"
                name="history"
                value={visibility}
                checked={settings.historyVisibility === visibility}
                disabled={busy || !may?.historyVisibility}
                onchange={() => apply({ kind: 'history_visibility', visibility })}
              />
              <span>{historyText[visibility]}</span>
            </label>
          {/each}
          <p class="note">
            A change applies to messages sent after it. Encrypted messages stay unreadable without
            their keys, whoever may read the history.
          </p>
        </fieldset>
      {:else}
        <section class="panel" aria-labelledby="room-details-heading">
          <h4 id="room-details-heading">Room details</h4>
          <dl class="details">
            <dt>Room ID</dt>
            <dd><bdi class="mono selectable">{settings.roomId}</bdi></dd>
            <dt>Room version</dt>
            <dd><bdi>{settings.roomVersion ?? 'Unknown'}</bdi></dd>
            <dt>Your role</dt>
            <dd>
              {own === null
                ? 'Creator, with every permission'
                : `${roleOf(own)}${roleOf(own).startsWith('Level') ? '' : ` (level ${own})`}`}
            </dd>
          </dl>
        </section>
        <section class="panel" aria-labelledby="permissions-heading">
          <h4 id="permissions-heading">Permissions</h4>
          <p>The role needed for each action. Members get the default level unless given a role.</p>
          {#if !may?.powerLevels}
            <p class="note">You cannot change permissions in this room.</p>
          {/if}
          {#each levelGroups as group (group.title)}
            <h5>{group.title}</h5>
            <ul class="level-list">
              {#each group.keys as key (key)}
                {@render levelRow(key)}
              {/each}
            </ul>
          {/each}
        </section>
      {/if}
      {/key}

      {#if confirming || failure || done}
        <div class="room-settings-feedback">
      {#if confirming}
        <div class="confirm" role="alertdialog" aria-labelledby="room-settings-question">
          <p id="room-settings-question">{confirming.question}</p>
          <button
            type="button"
            class="danger"
            disabled={busy}
            onclick={() => confirming && apply(confirming.change)}>{confirming.action}</button
          >
          <button type="button" class="secondary" disabled={busy} onclick={dismiss}>Cancel</button>
        </div>
      {/if}
      {#if failure}
        <p role="alert" class="error">{failure}</p>
      {:else if done}
        <p role="status" class="note">{done}</p>
      {/if}
        </div>
      {/if}
    </div>
  </div>
</dialog>
