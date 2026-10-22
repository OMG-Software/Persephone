<script lang="ts">
  import type {
    PendingMessage,
    SendFailure
  } from '../../crates/contracts/generated/contracts';

  let {
    pending,
    blocked,
    notEncrypted,
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
    notEncrypted: boolean;
    // Advanced when the draft was sent after a confirmation, to clear it.
    draftVersion: number;
    onsend: (body: string) => Promise<boolean>;
    onretry: (localId: number) => void;
    ondiscard: (localId: number) => void;
    onallow: (localId: number) => void;
  } = $props();

  // The core's limit is 16 KiB of UTF-8; this length is only a first stop.
  const MAX_DRAFT = 16 * 1024;
  let draft = $state('');
  let sending = $state(false);
  let lastVersion = 0;

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
</script>

{#if pending.length > 0}
  <ol class="pending" aria-label="Messages you sent">
    {#each pending as message (message.localId)}
      <li>
        <p dir="auto">{message.body}</p>
        <p class="note" role={message.state === 'failed' || message.state === 'unknown' ? 'alert' : null}>
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

<form class="composer" onsubmit={submit}>
  <label for="composer-text">{notEncrypted ? 'Message (not encrypted)' : 'Message'}</label>
  <textarea
    id="composer-text"
    rows="3"
    maxlength={MAX_DRAFT}
    bind:value={draft}
    disabled={blocked !== null}
    dir="auto"
  ></textarea>
  {#if blocked}
    <p class="note">{blocked}</p>
  {/if}
  <button type="submit" disabled={blocked !== null || sending || !draft.trim()}>
    {sending ? 'Sending…' : 'Send'}
  </button>
</form>
