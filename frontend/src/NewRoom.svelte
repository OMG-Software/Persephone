<script lang="ts">
  import { onMount } from 'svelte';
  import { createRoom } from './bridge';

  // Creates an encrypted, invite-only room with a name and invites people to
  // it by their exact Matrix IDs. The core creates nothing when one of them
  // has not set up encryption.
  let {
    onopen,
    onclose
  }: {
    onopen: (roomId: string) => void;
    onclose: () => void;
  } = $props();

  // The core's limits; these are only a first stop.
  const MAX_NAME = 255;
  const MAX_USER_ID = 255;
  const MAX_INVITEES = 10;

  let name = $state('');
  let person = $state('');
  let invite = $state<string[]>([]);
  let creating = $state(false);
  let failure = $state<string | null>(null);
  let dialog = $state<HTMLDialogElement | null>(null);

  const looksLikeUserId = (text: string) =>
    text.length <= MAX_USER_ID && /^@[^\s:]+:\S+$/.test(text);
  const roomName = $derived(name.trim());
  const nextPerson = $derived(person.trim());
  const canAdd = $derived(
    looksLikeUserId(nextPerson) && !invite.includes(nextPerson) && invite.length < MAX_INVITEES
  );

  const failures: Record<string, string> = {
    invalid_argument: 'Check the room name and the Matrix IDs. You cannot invite yourself.',
    core_unavailable: 'The homeserver could not be reached.',
    limit_exceeded: 'Another room or conversation is being set up. Try again in a moment.',
    locked: 'The session is locked.',
    not_found: 'Sign in to create a room.'
  };

  function add() {
    if (!canAdd) return;
    invite = [...invite, nextPerson];
    person = '';
  }

  // Enter in the person field adds them; it does not create the room.
  function personKey(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.isComposing) {
      event.preventDefault();
      add();
    }
  }

  function remove(userId: string) {
    invite = invite.filter((invited) => invited !== userId);
  }

  async function create(event: SubmitEvent) {
    event.preventDefault();
    if (creating || !roomName) return;
    creating = true;
    failure = null;
    try {
      const result = await createRoom(roomName, invite);
      if (result.kind === 'created') {
        onopen(result.roomId);
        onclose();
      } else if (result.kind === 'no_identity') {
        failure = `${result.userId} has not set up encryption, or the ID does not exist. Persephone creates only encrypted rooms, so nothing was created.`;
      } else {
        failure = 'The homeserver refused. Check the Matrix IDs.';
      }
    } catch (reason) {
      failure = failures[String(reason)] ?? 'That did not work.';
    } finally {
      creating = false;
    }
  }

  // Focus goes back to where the dialog was opened from when it closes.
  onMount(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.showModal();
    return () => {
      dialog?.close();
      previous?.focus();
    };
  });

  // Escape closes the dialog, unless the room is being created.
  function cancel(event: Event) {
    event.preventDefault();
    if (!creating) onclose();
  }
</script>

<dialog class="picker new-room" aria-labelledby="new-room-heading" bind:this={dialog} oncancel={cancel}>
  <form onsubmit={create}>
    <h2 id="new-room-heading">New room</h2>
    <label for="new-room-name">Room name</label>
    <input
      id="new-room-name"
      class="field"
      type="text"
      maxlength={MAX_NAME}
      autocomplete="off"
      dir="auto"
      bind:value={name}
    />
    <label for="new-room-person">Invite people</label>
    <div class="field-row">
      <input
        id="new-room-person"
        class="field"
        type="text"
        placeholder="@name:example.org"
        maxlength={MAX_USER_ID}
        spellcheck="false"
        autocomplete="off"
        bind:value={person}
        onkeydown={personKey}
      />
      <button type="button" class="secondary" disabled={!canAdd} onclick={add}>Add</button>
    </div>
    {#if invite.length > 0}
      <ul class="invitees" aria-label="People to invite">
        {#each invite as userId (userId)}
          <li>
            <bdi>{userId}</bdi>
            <button type="button" class="secondary" disabled={creating} onclick={() => remove(userId)}>
              Remove <span class="visually-hidden"><bdi>{userId}</bdi></span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}
    <p class="note">
      The room is encrypted and invite-only. Everyone you invite must have set up encryption. You can
      invite up to {MAX_INVITEES} people now, and more later.
    </p>
    {#if failure}
      <p role="alert" class="error">{failure}</p>
    {/if}
    <div class="actions">
      <button type="button" class="secondary" disabled={creating} onclick={onclose}>Cancel</button>
      <button type="submit" class="primary" disabled={creating || !roomName}>
        {creating ? 'Creating…' : 'Create'}
      </button>
    </div>
  </form>
</dialog>
