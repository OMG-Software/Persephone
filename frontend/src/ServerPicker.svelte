<script lang="ts">
  import { onMount } from 'svelte';
  import { discoverHomeserver, type HomeserverFound } from './bridge';

  // Chooses the homeserver to sign in to. Only the address leaves this
  // dialog; the core checks it again before it connects. A server name such
  // as example.org is looked up through its .well-known file first, and the
  // address found is shown before it is used (ADR-0027).
  let {
    current,
    onchoose,
    oncancel
  }: {
    current: string;
    onchoose: (homeserver: string) => void;
    oncancel: () => void;
  } = $props();

  const MATRIX_ORG = 'https://matrix.org';

  // svelte-ignore state_referenced_locally
  let choice = $state<'matrix_org' | 'other'>(current === MATRIX_ORG ? 'matrix_org' : 'other');
  // svelte-ignore state_referenced_locally
  let other = $state(current === MATRIX_ORG ? 'https://' : current);
  let dialog = $state<HTMLDialogElement | null>(null);
  // The homeserver found for the name typed, and the name it was found for.
  let found = $state<(HomeserverFound & { name: string }) | null>(null);
  let finding = $state(false);
  let findFailure = $state<string | null>(null);

  const findFailures: Record<string, string> = {
    not_found: 'No Matrix server answers at that name.',
    rejected: "That server's Matrix settings (its .well-known file) are not valid.",
    core_unavailable: 'That server could not be reached.',
    invalid_argument:
      'Enter a server name such as example.org, or a full address starting with https://.',
    limit_exceeded: 'A server is already being looked up.'
  };

  onMount(() => {
    dialog?.showModal();
  });

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (choice === 'matrix_org') {
      onchoose(MATRIX_ORG);
      return;
    }
    const typed = other.trim();
    if (found && found.name === typed) {
      onchoose(found.homeserver);
      return;
    }
    if (/^https?:\/\//i.test(typed)) {
      onchoose(typed);
      return;
    }
    if (finding) return;
    finding = true;
    findFailure = null;
    try {
      found = { ...(await discoverHomeserver(typed)), name: typed };
    } catch (reason) {
      found = null;
      findFailure = findFailures[String(reason)] ?? 'The server could not be looked up.';
    } finally {
      finding = false;
    }
  }

  // Escape closes the dialog without a choice.
  function cancel(event: Event) {
    event.preventDefault();
    oncancel();
  }
</script>

<dialog class="picker" aria-labelledby="picker-heading" bind:this={dialog} oncancel={cancel}>
  <form onsubmit={submit}>
    <h2 id="picker-heading">Choose a server</h2>
    <p class="auth-note">
      Your account lives on a server. You can talk to people on any other server, so this only
      decides who hosts your data.
    </p>
    <label class="choice">
      <input type="radio" name="server" value="matrix_org" bind:group={choice} />
      <span class="choice-name">matrix.org</span>
      <span class="choice-text">Free and public. The largest server, run by the Matrix.org Foundation.</span>
    </label>
    <label class="choice">
      <input type="radio" name="server" value="other" bind:group={choice} />
      <span class="choice-name">Another server</span>
      <span class="choice-text">Your workplace or community server.</span>
    </label>
    {#if choice === 'other'}
      <label for="homeserver" class="visually-hidden">Homeserver address</label>
      <input
        id="homeserver"
        class="field mono"
        type="text"
        required
        maxlength="2048"
        autocomplete="url"
        spellcheck="false"
        placeholder="example.org or https://chat.example.org"
        bind:value={other}
        oninput={() => {
          found = null;
          findFailure = null;
        }}
      />
      {#if found && found.name === other.trim()}
        <p class="auth-note" role="status">
          Found <bdi class="mono">{found.homeserver}</bdi>{found.fromWellKnown
            ? ', named by the server\'s .well-known file'
            : ''}. Check that it is the server you expect before you sign in.
        </p>
      {/if}
      {#if findFailure}
        <p class="error" role="alert">{findFailure}</p>
      {/if}
    {/if}
    <div class="actions">
      <button type="button" class="secondary" onclick={oncancel}>Cancel</button>
      <button type="submit" class="primary" disabled={finding}>
        {choice === 'matrix_org'
          ? 'Use matrix.org'
          : finding
            ? 'Looking up…'
            : found && found.name === other.trim()
              ? 'Use this server'
              : /^https?:\/\//i.test(other.trim())
                ? 'Use this server'
                : 'Find server'}
      </button>
    </div>
  </form>
</dialog>
