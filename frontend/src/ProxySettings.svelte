<script lang="ts">
  import { onMount } from 'svelte';
  import { proxySetting, setProxySetting } from './bridge';

  // The profile's SOCKS5 proxy (ADR-0028). It belongs to this computer's
  // profile, not the account, so it can be set before signing in; every
  // connection Persephone makes then goes through it.
  let current = $state<string | null>(null);
  let typed = $state('');
  let loaded = $state(false);
  // The record could not be read: Persephone does not connect until it is
  // replaced or removed, which saving does without reading it.
  let unreadable = $state(false);
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let saved = $state<string | null>(null);

  const failures: Record<string, string> = {
    invalid_argument: 'Enter the proxy as host:port, for example 127.0.0.1:9050.',
    locked: 'The profile is locked.',
    core_internal: 'The proxy setting could not be read or saved.',
    core_unavailable: 'The proxy setting cannot be changed now.'
  };

  // Reads the setting. Only `core_unavailable` (the record cannot be read)
  // enables replacing it unread; a busy or locked session leaves the form
  // closed, and Try again reads it once more.
  let retry = $state(false);
  async function load() {
    retry = false;
    failure = null;
    try {
      current = (await proxySetting()).proxy;
      typed = current ?? '';
      loaded = true;
    } catch (reason) {
      const code = String(reason);
      if (code === 'core_unavailable') {
        unreadable = true;
        loaded = true;
        failure =
          'The proxy setting could not be read, so Persephone does not connect. Save a proxy or remove the setting.';
        return;
      }
      retry = code !== 'locked';
      failure =
        code === 'locked'
          ? failures.locked
          : code === 'limit_exceeded'
            ? 'Persephone is busy. Try again in a moment.'
            : 'The proxy setting could not be read.';
    }
  }

  onMount(load);

  async function save(proxy: string | null) {
    if (busy) return;
    busy = true;
    failure = null;
    saved = null;
    try {
      await setProxySetting(proxy);
      current = proxy;
      typed = proxy ?? '';
      unreadable = false;
      saved = proxy
        ? 'Saved. Persephone uses the proxy from its next start.'
        : 'Removed. Persephone connects directly from its next start.';
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The proxy could not be saved.';
    } finally {
      busy = false;
    }
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    const proxy = typed.trim();
    save(proxy === '' ? null : proxy);
  }
</script>

<form class="proxy-settings" onsubmit={submit}>
  <label for="proxy-address">SOCKS5 proxy</label>
  <input
    id="proxy-address"
    class="field mono"
    type="text"
    maxlength="300"
    spellcheck="false"
    autocomplete="off"
    placeholder="127.0.0.1:9050"
    disabled={!loaded || busy}
    bind:value={typed}
  />
  <p class="note">
    Every connection Persephone makes goes through it, and it looks up server names, so this
    computer's network does not see which servers you use. Tor's own proxy is usually
    127.0.0.1:9050. If the proxy does not answer, Persephone does not connect. Links you open go
    through your browser's own settings.
  </p>
  <div class="actions">
    <button type="submit" class="secondary" disabled={!loaded || busy}>Save proxy</button>
    {#if current || unreadable}
      <button type="button" class="secondary" disabled={busy} onclick={() => save(null)}
        >Remove proxy</button
      >
    {/if}
  </div>
  {#if failure}
    <p role="alert" class="error">{failure}</p>
    {#if retry}
      <button type="button" class="secondary" onclick={load}>Try again</button>
    {/if}
  {:else if saved}
    <p role="status" class="note">{saved}</p>
  {/if}
</form>
