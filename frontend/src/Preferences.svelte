<script lang="ts">
  import { onMount } from 'svelte';
  import { notificationLevel, setNotificationLevel, type AccountSettings, type NotificationLevel } from './bridge';
  import { notificationLevelText } from './accountChanges';
  import { CHAT_TEXT_SIZES, INTERFACE_TEXT_SIZES } from './preferences';

  // This account's preferences. The settings are kept in its encrypted
  // profile through the caller; the notification level is the account's
  // push rules on the homeserver, read and set here.
  let {
    settings,
    failure,
    mac,
    onchange
  }: {
    settings: AccountSettings;
    failure: string | null;
    mac: boolean;
    // Only the settings a control changes; the caller lays them over the
    // stored ones.
    onchange: (patch: Partial<AccountSettings>) => void;
  } = $props();

  let level = $state<NotificationLevel | null>(null);
  let levelBusy = $state(false);
  let levelFailure = $state<string | null>(null);

  const levelFailures: Record<string, string> = {
    core_unavailable: 'The homeserver could not be reached, or did not take the change.',
    limit_exceeded: 'The notification level is already being changed.',
    locked: 'The session is locked.'
  };

  onMount(async () => {
    try {
      level = (await notificationLevel()).level;
    } catch (reason) {
      levelFailure = levelFailures[String(reason)] ?? 'The notification level could not be read.';
    }
  });

  // The homeserver applies a level once the change is sent; the next sync
  // brings it back, so the choice is shown at once.
  async function chooseLevel(next: Exclude<NotificationLevel, 'custom'>) {
    if (levelBusy || next === level) return;
    levelBusy = true;
    levelFailure = null;
    const previous = level;
    level = next;
    try {
      await setNotificationLevel(next);
    } catch (reason) {
      level = previous;
      levelFailure = levelFailures[String(reason)] ?? 'The notification level could not be changed.';
    } finally {
      levelBusy = false;
    }
  }

  const choices: Exclude<NotificationLevel, 'custom'>[] = ['all', 'mentions', 'none'];
</script>

<section class="settings-page" aria-labelledby="preferences-heading">
  <h2 id="preferences-heading">Preferences</h2>
  <p class="lede">
    These settings belong to this account. The notification level is kept on your homeserver, so
    your other apps follow it too; the rest is kept in this computer's encrypted profile.
  </p>
  <div class="panel">
    <h3>Messages</h3>
    <label class="switch-row">
      <input
        type="checkbox"
        role="switch"
        checked={settings.enterSends}
        onchange={(event) => onchange({ enterSends: event.currentTarget.checked })}
      />
      <span>Enter sends the message</span>
    </label>
    <p class="note">
      {settings.enterSends
        ? 'Enter sends. Shift+Enter starts a new line.'
        : `Enter starts a new line. ${mac ? '⌘Enter' : 'Ctrl+Enter'} sends.`}
    </p>
  </div>
  <div class="panel">
    <h3>Text size</h3>
    <label for="chat-text-size">Message text</label>
    <select
      id="chat-text-size"
      class="field"
      value={settings.chatTextPercent}
      onchange={(event) =>
        onchange({ chatTextPercent: Number(event.currentTarget.value) })}
    >
      {#each CHAT_TEXT_SIZES as size (size)}
        <option value={size}>{size}%{size === 100 ? ' (normal)' : ''}</option>
      {/each}
    </select>
    <label for="interface-text-size">Interface</label>
    <select
      id="interface-text-size"
      class="field"
      value={settings.interfaceTextPercent}
      onchange={(event) =>
        onchange({ interfaceTextPercent: Number(event.currentTarget.value) })}
    >
      {#each INTERFACE_TEXT_SIZES as size (size)}
        <option value={size}>{size}%{size === 100 ? ' (normal)' : ''}</option>
      {/each}
    </select>
    <p class="note">The interface size scales everything else: menus, lists, buttons and settings.</p>
  </div>
  <div class="panel">
    <h3>Notifications</h3>
    <fieldset class="choices" disabled={level === null || levelBusy}>
      <legend>Notify me of</legend>
      {#each choices as choice (choice)}
        <label class="switch-row">
          <input
            type="radio"
            name="notification-level"
            value={choice}
            checked={level === choice}
            onchange={() => chooseLevel(choice)}
          />
          <span>{notificationLevelText[choice]}</span>
        </label>
      {/each}
    </fieldset>
    {#if level === 'custom'}
      <p class="note">
        Now: {notificationLevelText.custom}. Choosing a level here sets it for every kind of room.
      </p>
    {:else if level === null && !levelFailure}
      <p class="note" role="status">Reading the notification level…</p>
    {/if}
    <p class="note">Rooms with their own setting keep it. Mentions of you still count as mentions.</p>
    {#if levelFailure}
      <p role="alert" class="error">{levelFailure}</p>
    {/if}
    <label class="switch-row">
      <input
        type="checkbox"
        role="switch"
        checked={settings.desktopNotifications}
        onchange={(event) =>
          onchange({ desktopNotifications: event.currentTarget.checked })}
      />
      <span>Show desktop notifications</span>
    </label>
    <p class="note">
      A notification shows who wrote and in which room, never the message itself. It shows only while
      Persephone's window is not in front.
    </p>
  </div>
  <div class="panel">
    <h3>Privacy</h3>
    <label class="switch-row">
      <input
        type="checkbox"
        role="switch"
        checked={settings.sendTyping}
        onchange={(event) => onchange({ sendTyping: event.currentTarget.checked })}
      />
      <span>Tell people when you are typing</span>
    </label>
    <label class="switch-row">
      <input
        type="checkbox"
        role="switch"
        checked={settings.sendReadReceipts}
        onchange={(event) => onchange({ sendReadReceipts: event.currentTarget.checked })}
      />
      <span>Show people how far you have read</span>
    </label>
    <p class="note">
      Both are off by default: each tells the homeserver and everyone in the room when you are
      active. You still see other people's. Your other devices always learn what you have read.
    </p>
  </div>
  {#if failure}
    <p role="alert" class="error">{failure}</p>
  {/if}
</section>
