<script lang="ts">
  import { onMount } from 'svelte';

  // Chooses the homeserver to sign in to. Only the address leaves this
  // dialog; the core checks it again before it connects.
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

  onMount(() => {
    dialog?.showModal();
  });

  function submit(event: SubmitEvent) {
    event.preventDefault();
    onchoose(choice === 'matrix_org' ? MATRIX_ORG : other.trim());
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
        type="url"
        required
        maxlength="2048"
        autocomplete="url"
        spellcheck="false"
        placeholder="https://chat.example.org"
        bind:value={other}
      />
    {/if}
    <div class="actions">
      <button type="button" class="secondary" onclick={oncancel}>Cancel</button>
      <button type="submit" class="primary">
        {choice === 'matrix_org' ? 'Use matrix.org' : 'Use this server'}
      </button>
    </div>
  </form>
</dialog>
