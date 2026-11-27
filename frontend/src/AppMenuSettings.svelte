<script lang="ts">
  import { onMount } from 'svelte';
  import { appMenuStatus, setAppMenu, type AppMenuState } from './bridge';
  import { appMenuFailure, appMenuView } from './appMenu';

  // Persephone's entry in the desktop's app menu, for a Linux AppImage
  // (ADR-0072). The panel shows only when the core reports one. Adding shows
  // the core's own question, outside this page, and the user can refuse there.
  let menuState = $state<AppMenuState>('unavailable');
  let busy = $state(false);
  let failure = $state<string | null>(null);
  const view = $derived(appMenuView(menuState));

  onMount(async () => {
    try {
      menuState = (await appMenuStatus()).state;
    } catch {
      // Nothing to show: the panel stays hidden.
    }
  });

  async function change(on: boolean) {
    if (busy) return;
    busy = true;
    failure = null;
    try {
      menuState = (await setAppMenu(on)).state;
    } catch (reason) {
      failure = appMenuFailure(reason);
    } finally {
      busy = false;
    }
  }
</script>

{#if view}
  <div class="panel">
    <h3>App menu</h3>
    <p class="note">{view.note}</p>
    {#if view.actions.length > 0}
      <div class="actions">
        {#each view.actions as action (action.label)}
          <button type="button" class="secondary" disabled={busy} onclick={() => change(action.on)}
            >{action.label}</button
          >
        {/each}
      </div>
    {/if}
    {#if failure}
      <p role="alert" class="error">{failure}</p>
    {/if}
  </div>
{/if}
