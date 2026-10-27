<script lang="ts">
  import { onMount } from 'svelte';
  import { displayName, setDisplayName } from './bridge';

  // This account's display name, as the homeserver has it. Others see it in
  // every room; an empty name removes it.
  let { userId }: { userId: string } = $props();

  // The core's limit, in bytes; this length is only a first stop.
  const MAX_NAME = 256;

  let current = $state<string | null>(null);
  let loaded = $state(false);
  let name = $state('');
  let saving = $state(false);
  let failure = $state<string | null>(null);
  let saved = $state(false);

  const failures: Record<string, string> = {
    invalid_argument: 'Use a shorter name without line breaks or control characters.',
    core_unavailable: 'The homeserver could not be reached.',
    rejected: 'The homeserver refused that name.',
    limit_exceeded: 'The name is already being changed. Try again in a moment.',
    locked: 'The session is locked.'
  };

  onMount(async () => {
    try {
      current = (await displayName()).displayName;
      name = current ?? '';
      loaded = true;
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The display name could not be read.';
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving) return;
    saving = true;
    saved = false;
    failure = null;
    try {
      current = (await setDisplayName(name)).displayName;
      name = current ?? '';
      saved = true;
    } catch (reason) {
      failure = failures[String(reason)] ?? 'That did not work.';
    } finally {
      saving = false;
    }
  }
</script>

<section class="settings-page" aria-labelledby="profile-heading">
  <h2 id="profile-heading">Profile</h2>
  <p class="lede">Your display name is shown to everyone in your rooms, beside <bdi>{userId}</bdi>.</p>
  <form class="panel" onsubmit={save}>
    <label for="display-name">Display name</label>
    <input
      id="display-name"
      class="field"
      type="text"
      maxlength={MAX_NAME}
      autocomplete="off"
      dir="auto"
      disabled={!loaded || saving}
      bind:value={name}
    />
    <p class="note">Leave it empty to remove your display name.</p>
    <div class="actions">
      <button
        type="submit"
        class="primary"
        disabled={!loaded || saving || name.trim() === (current ?? '')}
      >
        {saving ? 'Saving…' : 'Save'}
      </button>
    </div>
    {#if saved}
      <p class="note" role="status">Saved.</p>
    {/if}
    {#if failure}
      <p role="alert" class="error">{failure}</p>
    {/if}
  </form>
</section>
