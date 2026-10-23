<script lang="ts">
  import type {
    PendingMessage,
    RoomEncryption,
    SendFailure
  } from '../../crates/contracts/generated/contracts';
  import EncryptionIcon from './EncryptionIcon.svelte';
  import Icon from './Icon.svelte';
  import { isolate } from './people';
  import { clockTime, fullTime, toDate } from './time';

  let {
    pending,
    blocked,
    encryption,
    roomName,
    draft = $bindable(''),
    draftVersion,
    onsend,
    onretry,
    ondiscard,
    onallow
  }: {
    // Messages sent from this device that the timeline does not show yet.
    pending: PendingMessage[];
    // Why sending is not possible here, or null.
    blocked: string | null;
    encryption: RoomEncryption;
    roomName: string;
    // The unsent text. The caller keeps it, so it survives while another
    // screen, such as a verification, replaces the room view.
    draft?: string;
    // Advanced when the draft was sent after a confirmation, to clear it.
    draftVersion: number;
    onsend: (body: string) => Promise<boolean>;
    onretry: (localId: number) => void;
    ondiscard: (localId: number) => void;
    onallow: (localId: number) => void;
  } = $props();

  // The core's limit is 16 KiB of UTF-8; this length is only a first stop.
  const MAX_DRAFT = 16 * 1024;
  let sending = $state(false);
  let explaining = $state(false);
  // Starts at the version the composer mounts with, so mounting again (after
  // settings or a verification) keeps the draft the caller kept.
  // svelte-ignore state_referenced_locally
  let lastVersion = draftVersion;
  let form = $state<HTMLFormElement | null>(null);

  const notEncrypted = $derived(encryption === 'not_encrypted');

  $effect(() => {
    if (draftVersion !== lastVersion) {
      lastVersion = draftVersion;
      draft = '';
    }
  });

  const failureText: Record<SendFailure, string> = {
    confirmation_required: 'Not sent: this room is not encrypted.',
    device_not_verified: 'Not sent: verify this device first.',
    identity_changed: 'Not sent: someone in this room changed their encryption identity.',
    not_joined: 'Not sent: you are not in this room.',
    rejected: 'Not sent: the homeserver refused it.',
    failed: 'Not sent.'
  };

  function stateText(message: PendingMessage): string {
    switch (message.state) {
      case 'queued':
        return 'Waiting to send…';
      case 'sending':
        return 'Sending…';
      case 'sent':
        return 'Sent.';
      case 'unknown':
        return 'Delivery unknown: the connection failed. Retrying cannot send it twice.';
      default:
        return message.failure ? (failureText[message.failure] ?? failureText.failed) : failureText.failed;
    }
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (sending || !draft.trim()) return;
    sending = true;
    try {
      if (await onsend(draft)) draft = '';
    } finally {
      sending = false;
    }
  }

  // Enter sends and Shift+Enter starts a new line. Not while an input method
  // is composing text.
  function keydown(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      form?.requestSubmit();
    }
  }
</script>

<div class="composer-area">
  {#if pending.length > 0}
    <ol class="pending" aria-label="Messages you sent">
      {#each pending as message (message.localId)}
        {@const queued = toDate(message.queuedAt)}
        <li>
          <p dir="auto">{message.body}</p>
          <p class="note" role={message.state === 'failed' || message.state === 'unknown' ? 'alert' : null}>
            {#if queued}
              <time datetime={queued.toISOString()} title={'Queued on this device ' + fullTime(queued)}
                >{clockTime(queued)}</time
              > ·
            {/if}
            {stateText(message)}
          </p>
          {#if message.state === 'failed' && message.failure === 'confirmation_required'}
            <button type="button" onclick={() => onallow(message.localId)}>Send unencrypted</button>
          {/if}
          {#if message.state === 'failed' || message.state === 'unknown'}
            <button type="button" onclick={() => onretry(message.localId)}>Retry</button>
          {/if}
          {#if message.state === 'failed' || message.state === 'unknown' || message.state === 'queued'}
            <button type="button" onclick={() => ondiscard(message.localId)}>Discard</button>
          {/if}
        </li>
      {/each}
    </ol>
  {/if}

  <form class="composer" class:plain={notEncrypted} bind:this={form} onsubmit={submit}>
    {#if notEncrypted}
      <!-- The warning sits on the composer, where the risk is. -->
      <div class="plain-warning">
        <span aria-hidden="true"><EncryptionIcon encryption="not_encrypted" size="bar" /></span>
        <p>
          <strong>Not encrypted.</strong> Messages in this room can be read by the server and anyone
          who joins.
        </p>
        <button
          type="button"
          class="link"
          aria-expanded={explaining}
          aria-controls="plain-more"
          onclick={() => (explaining = !explaining)}
        >
          Learn more
        </button>
        {#if explaining}
          <p id="plain-more" class="more">
            Your homeserver stores what you send here as readable text, and so does every other
            server with people in this room. People who join later can read the history too. A room
            admin can turn encryption on; it cannot be turned off again.
          </p>
        {/if}
      </div>
    {/if}
    <div class="composer-box">
      <label for="composer-text" class="visually-hidden"
        >{notEncrypted ? 'Message (not encrypted)' : 'Message'}</label
      >
      <textarea
        id="composer-text"
        rows="1"
        maxlength={MAX_DRAFT}
        placeholder={'Message #' + isolate(roomName)}
        bind:value={draft}
        disabled={blocked !== null}
        dir="auto"
        onkeydown={keydown}
      ></textarea>
      {#if encryption === 'encrypted'}
        <span class="enc-note"><Icon name="lock" />Encrypted</span>
      {/if}
      <button
        type="submit"
        class="send"
        aria-label={sending ? 'Sending…' : 'Send'}
        title="Send"
        disabled={blocked !== null || sending || !draft.trim()}
      >
        <Icon name="send" />
      </button>
    </div>
    {#if blocked}
      <p class="note">{blocked}</p>
    {/if}
  </form>
</div>
