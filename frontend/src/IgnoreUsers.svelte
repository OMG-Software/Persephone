<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { ignoredUsers, setIgnoredUser, type IgnoredUsers } from './bridge';
  import FeatureDialog from './FeatureDialog.svelte';
  let { onclose, onchanged }: { onclose: () => void; onchanged: () => void } = $props();
  let users = $state<IgnoredUsers | null>(null);
  let userId = $state('');
  let confirming = $state<{ userId: string; ignored: boolean } | null>(null);
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let active = true;
  onDestroy(() => { active = false; });
  async function load() {
    try { const reply = await ignoredUsers(); if (active) users = reply; }
    catch { if (active) failure = 'The ignored-user list could not be loaded.'; }
  }
  onMount(load);
  async function apply() {
    if (!confirming || busy) return;
    const asked = confirming;
    busy = true;
    failure = null;
    try {
      await setIgnoredUser(asked.userId, asked.ignored);
      if (!active) return;
      confirming = null;
      userId = '';
      onchanged();
      await load();
    } catch { if (active) failure = 'The ignore setting could not be changed. Check the exact Matrix ID and try again.'; }
    finally { if (active) busy = false; }
  }
</script>

<FeatureDialog title="Ignored users" {onclose}>
  <p>Ignoring hides that user's messages. It does not remove them from shared rooms or stop them from reading messages there. Room state changes still apply. Reports are separate.</p>
  <form onsubmit={(event) => { event.preventDefault(); confirming = { userId: userId.trim(), ignored: true }; }}>
    <label for="ignore-user-id">Exact Matrix ID, for example @name:example.org</label>
    <input id="ignore-user-id" bind:value={userId} maxlength="255" autocomplete="off" spellcheck="false" disabled={busy} />
    <button type="submit" class="secondary" disabled={busy || !/^@[^\s:]+:[^\s]+$/.test(userId.trim())}>Ignore user…</button>
  </form>
  {#if confirming}
    <div class="confirm" role="group" aria-label="Confirm ignore change">
      <p>{confirming.ignored ? 'Ignore' : 'Unignore'} <bdi>{confirming.userId}</bdi>?</p>
      <button type="button" class="primary" disabled={busy} onclick={apply}>{busy ? 'Saving…' : 'Confirm'}</button>
      <button type="button" class="secondary" disabled={busy} onclick={() => (confirming = null)}>Cancel</button>
    </div>
  {/if}
  {#if failure}<p role="alert" class="error">{failure}</p><button type="button" class="secondary" disabled={busy} onclick={load}>Reload list</button>{/if}
  {#if users}
    <ul>{#each users.users as id (id)}<li><bdi>{id}</bdi> <button type="button" class="link" disabled={busy} onclick={() => (confirming = { userId: id, ignored: false })}>Unignore…</button></li>{/each}</ul>
    {#if users.users.length === 0}<p>No ignored users.</p>{/if}
    {#if users.truncated}<p class="note">Only part of the ignored-user list is shown.</p>{/if}
  {:else}<p role="status">Loading ignored users…</p>{/if}
</FeatureDialog>
