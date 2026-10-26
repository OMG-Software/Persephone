<script lang="ts">
  import { onMount } from 'svelte';
  import { searchUsers, startConversation, type UserMatch } from './bridge';

  // Starts a direct-message conversation with one person, named by their
  // exact Matrix ID. Directory search is off each time the dialog opens:
  // while it is on, the homeserver sees what is typed.
  let {
    onopen,
    onclose
  }: {
    onopen: (roomId: string) => void;
    onclose: () => void;
  } = $props();

  // The Matrix limit on user IDs, and the pause before a directory search.
  const MAX_USER_ID = 255;
  const SEARCH_DELAY_MS = 400;
  const MIN_SEARCH_LENGTH = 2;

  let userId = $state('');
  let directory = $state(false);
  let matches = $state<UserMatch[]>([]);
  let moreMatches = $state(false);
  let searchFailed = $state(false);
  let starting = $state(false);
  let failure = $state<string | null>(null);
  let dialog = $state<HTMLDialogElement | null>(null);
  // A search reply is shown only if it answers the newest term.
  let searchRequest = 0;

  const looksLikeUserId = (text: string) =>
    text.length <= MAX_USER_ID && /^@[^\s:]+:\S+$/.test(text);
  const target = $derived(userId.trim());

  const failures: Record<string, string> = {
    invalid_argument: 'That is not a Matrix ID you can start a conversation with.',
    core_unavailable: 'The homeserver could not be reached.',
    limit_exceeded: 'Another conversation is being started. Try again in a moment.',
    locked: 'The session is locked.',
    not_found: 'Sign in to start a conversation.'
  };

  $effect(() => {
    const term = target;
    const on = directory;
    const request = ++searchRequest;
    searchFailed = false;
    if (!on || term.length < MIN_SEARCH_LENGTH) {
      matches = [];
      moreMatches = false;
      return;
    }
    let timer: ReturnType<typeof setTimeout>;
    // The core runs one search at a time: while an earlier one is still
    // running, this one waits and tries again instead of failing.
    const search = async () => {
      try {
        const found = await searchUsers(term);
        if (request === searchRequest) {
          matches = found.users;
          moreMatches = found.truncated;
        }
      } catch (reason) {
        if (request !== searchRequest) return;
        if (String(reason) === 'limit_exceeded') {
          timer = setTimeout(search, SEARCH_DELAY_MS);
        } else {
          matches = [];
          searchFailed = true;
        }
      }
    };
    timer = setTimeout(search, SEARCH_DELAY_MS);
    return () => clearTimeout(timer);
  });

  async function start(event: SubmitEvent) {
    event.preventDefault();
    if (starting || !looksLikeUserId(target)) return;
    starting = true;
    failure = null;
    try {
      const result = await startConversation(target);
      if (result.kind === 'created' || result.kind === 'existing') {
        onopen(result.roomId);
        onclose();
      } else if (result.kind === 'no_identity') {
        failure =
          'This person has not set up encryption, or the ID does not exist. Persephone starts only encrypted conversations.';
      } else {
        failure = 'The homeserver refused. Check the Matrix ID.';
      }
    } catch (reason) {
      failure = failures[String(reason)] ?? 'That did not work.';
    } finally {
      starting = false;
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

  // Escape closes the dialog, unless a conversation is being started.
  function cancel(event: Event) {
    event.preventDefault();
    if (!starting) onclose();
  }
</script>

<dialog
  class="picker new-conversation"
  aria-labelledby="new-conversation-heading"
  bind:this={dialog}
  oncancel={cancel}
>
  <form onsubmit={start}>
    <h2 id="new-conversation-heading">New conversation</h2>
    <label for="new-conversation-id">Matrix ID</label>
    <input
      id="new-conversation-id"
      class="field"
      type="text"
      placeholder="@name:example.org"
      maxlength={MAX_USER_ID}
      spellcheck="false"
      autocomplete="off"
      bind:value={userId}
    />
    <label class="switch-row">
      <input type="checkbox" role="switch" bind:checked={directory} />
      <span>Search the user directory</span>
    </label>
    <p class="note">
      {directory
        ? 'Your homeserver sees what you type here.'
        : 'Off: nothing is sent until you press Start.'}
    </p>
    {#if directory && matches.length > 0}
      <ul class="directory-results" aria-label="Directory matches">
        {#each matches as match (match.userId)}
          <li>
            <button type="button" class="directory-match" onclick={() => (userId = match.userId)}>
              {#if match.displayName}
                <bdi class="match-name">{match.displayName}</bdi>
              {/if}
              <bdi class="match-id">{match.userId}</bdi>
            </button>
          </li>
        {/each}
      </ul>
      {#if moreMatches}
        <p class="note">More people match. Type more of the name.</p>
      {/if}
    {:else if directory && searchFailed}
      <p class="note">The directory could not be searched.</p>
    {/if}
    {#if failure}
      <p role="alert" class="error">{failure}</p>
    {/if}
    <div class="actions">
      <button type="button" class="secondary" disabled={starting} onclick={onclose}>Cancel</button>
      <button type="submit" class="primary" disabled={starting || !looksLikeUserId(target)}>
        {starting ? 'Starting…' : 'Start'}
      </button>
    </div>
  </form>
</dialog>
