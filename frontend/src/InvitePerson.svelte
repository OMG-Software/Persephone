<script lang="ts">
  import { onMount } from 'svelte';
  import { inviteUser } from './bridge';

  // Invites one person, by their exact Matrix ID, to the open room. In an
  // encrypted room the core refuses someone who has not set up encryption.
  let {
    roomId,
    roomName,
    onclose
  }: {
    roomId: string;
    // Untrusted: shown as isolated text.
    roomName: string;
    onclose: () => void;
  } = $props();

  const MAX_USER_ID = 255;

  let userId = $state('');
  let inviting = $state(false);
  let failure = $state<string | null>(null);
  // The people invited while the dialog is open.
  let invited = $state<string[]>([]);
  let dialog = $state<HTMLDialogElement | null>(null);

  const looksLikeUserId = (text: string) =>
    text.length <= MAX_USER_ID && /^@[^\s:]+:\S+$/.test(text);
  const target = $derived(userId.trim());

  const failures: Record<string, string> = {
    invalid_argument: 'That is not a Matrix ID you can invite.',
    core_unavailable: 'The homeserver could not be reached.',
    limit_exceeded: 'Another invitation is being sent. Try again in a moment.',
    locked: 'The session is locked.',
    not_found: 'You are no longer in this room.'
  };

  async function invite(event: SubmitEvent) {
    event.preventDefault();
    if (inviting || !looksLikeUserId(target)) return;
    inviting = true;
    failure = null;
    try {
      const result = await inviteUser(roomId, target);
      if (result.kind === 'invited') {
        invited = [...invited, target];
        userId = '';
      } else if (result.kind === 'no_identity') {
        failure =
          'This person has not set up encryption, or the ID does not exist. They could not read this encrypted room.';
      } else {
        failure = 'The homeserver refused. They may already be in the room, or you may not invite here.';
      }
    } catch (reason) {
      failure = failures[String(reason)] ?? 'That did not work.';
    } finally {
      inviting = false;
    }
  }

  onMount(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.showModal();
    return () => {
      dialog?.close();
      previous?.focus();
    };
  });

  function cancel(event: Event) {
    event.preventDefault();
    if (!inviting) onclose();
  }
</script>

<dialog
  class="picker invite-person"
  aria-labelledby="invite-person-heading"
  bind:this={dialog}
  oncancel={cancel}
>
  <form onsubmit={invite}>
    <h2 id="invite-person-heading">Invite to <bdi>{roomName}</bdi></h2>
    <label for="invite-person-id">Matrix ID</label>
    <input
      id="invite-person-id"
      class="field"
      type="text"
      placeholder="@name:example.org"
      maxlength={MAX_USER_ID}
      spellcheck="false"
      autocomplete="off"
      bind:value={userId}
    />
    {#if invited.length > 0}
      <p class="note" role="status">
        Invited:
        {#each invited as person, index (index)}<bdi>{person}</bdi>{index < invited.length - 1
            ? ', '
            : ''}{/each}
      </p>
    {/if}
    {#if failure}
      <p role="alert" class="error">{failure}</p>
    {/if}
    <div class="actions">
      <button type="button" class="secondary" disabled={inviting} onclick={onclose}>Done</button>
      <button type="submit" class="primary" disabled={inviting || !looksLikeUserId(target)}>
        {inviting ? 'Inviting…' : 'Invite'}
      </button>
    </div>
  </form>
</dialog>
