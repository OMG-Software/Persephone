<script lang="ts">
  import { onMount } from 'svelte';
  import { displayName, setDisplayName } from './bridge';

  // This account's display name, as the homeserver has it. Others see it in
  // every room; an empty name removes it. With none set, the field is empty
  // and nothing is sent until a name is typed.
  let {
    userId,
    onsaved = () => {}
  }: {
    userId: string;
    // The account the save was made for, and the name now set, or null when
    // none is. The account may have changed while the save was running.
    onsaved?: (userId: string, name: string | null) => void;
  } = $props();

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

  // Reading sends no name, so a refusal there is not about one.
  const readFailures: Record<string, string> = {
    core_unavailable: failures.core_unavailable,
    locked: failures.locked
  };

  // Only a change is sent: an unset name left empty sends nothing.
  const changed = $derived(name.trim() !== (current ?? ''));

  onMount(async () => {
    try {
      current = (await displayName()).displayName;
      name = current ?? '';
      loaded = true;
    } catch (reason) {
      failure = readFailures[String(reason)] ?? 'The display name could not be read.';
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving || !changed) return;
    const account = userId;
    saving = true;
    saved = false;
    failure = null;
    try {
      current = (await setDisplayName(name)).displayName;
      name = current ?? '';
      saved = true;
      onsaved(account, current);
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
    {#if loaded && current === null}
      <p class="note">No display name is set, so others see only <bdi>{userId}</bdi>.</p>
    {:else}
      <p class="note">Leave it empty to remove your display name.</p>
    {/if}
    <div class="actions">
      <button
        type="submit"
        class="primary"
        disabled={!loaded || saving || !changed}
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
