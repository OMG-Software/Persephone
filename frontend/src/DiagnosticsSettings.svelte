<script lang="ts">
  import { onMount } from 'svelte';
  import { diagnosticsStatus, onDiagnosticsChange, setDiagnostics } from './bridge';

  // The diagnostic log (ADR-0058). It is off whenever Persephone starts. Turning
  // it on shows the core's own warning, outside this page, and the user can
  // refuse there. The core writes the file; the page only shows where it is.
  let on = $state(false);
  let full = $state(false);
  let available = $state(false);
  let path = $state<string | null>(null);
  let loaded = $state(false);
  let busy = $state(false);
  let failure = $state<string | null>(null);

  const failures: Record<string, string> = {
    // The host sends these two codes (`page_error` in src-tauri); the core's
    // `internal`, such as a file that could not be opened, arrives as
    // `core_unavailable` too.
    limit_exceeded: 'The diagnostic log warning is already open.',
    core_unavailable: 'The diagnostic log cannot be changed now, or its file could not be opened.'
  };

  async function load() {
    try {
      const status = await diagnosticsStatus();
      on = status.on;
      full = status.full;
      available = status.available;
      path = status.path;
      loaded = true;
    } catch {
      failure = 'The diagnostic log state could not be read.';
    }
  }

  function retry() {
    failure = null;
    load();
  }

  onMount(() => {
    load();
    // The core can change it too (for example a refusal in the warning).
    return onDiagnosticsChange(() => load());
  });

  // The file can reach its size limit while the log stays on, and the core
  // sends no change for that: read the state again while it is on and not
  // full, so the panel shows when writing stopped.
  const FULL_CHECK_MS = 10_000;
  $effect(() => {
    if (!on || full) return;
    const timer = setInterval(load, FULL_CHECK_MS);
    return () => clearInterval(timer);
  });

  async function change(value: boolean) {
    if (busy) return;
    busy = true;
    failure = null;
    try {
      on = await setDiagnostics(value);
      // The core's reply names no file; read it again.
      await load();
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The diagnostic log could not be changed.';
    } finally {
      busy = false;
    }
  }
</script>

<div class="diagnostics-settings">
  <p class="note">
    For finding faults. While it is on, Persephone writes what it does to a file on this computer:
    server, room, user and device names and IDs, the type and time of each event, call and
    connection steps, and errors. Its own records of events hold no message text, but the libraries
    it uses can add their own text. It is made to never write passwords, access tokens or keys, but
    no filter can be complete. Read the file before you share it, and share it only with someone you
    trust. It is off each time Persephone starts.
  </p>
  {#if loaded && !available}
    <p class="note">Not available: this start has no profile folder for the file.</p>
  {:else}
    <div class="actions">
      {#if on}
        <button type="button" class="secondary" disabled={busy} onclick={() => change(false)}
          >Turn off diagnostic log</button
        >
      {:else}
        <button type="button" class="secondary" disabled={!loaded || busy} onclick={() => change(true)}
          >Turn on diagnostic log…</button
        >
      {/if}
    </div>
  {/if}
  <!-- From the core's state, not from this panel's clicks: the file of this
       run stays until the next start, also after the panel was closed. -->
  {#if path}
    <p class="note">File: <span class="mono diagnostics-path">{path}</span></p>
    {#if full}
      <p role="status" class="note">
        The file reached its size limit (256 MiB). Nothing more is written until Persephone next
        starts.
      </p>
    {/if}
    <p class="note">
      {on ? 'On.' : 'Off. The file keeps what was written.'} When Persephone next starts, it overwrites
      the file and deletes it. On SSDs and some file systems, overwriting cannot guarantee that the old
      data is gone from the disk.
    </p>
  {/if}
  {#if failure}
    <p role="alert" class="error">{failure}</p>
    {#if !loaded}
      <!-- Nothing else reads the state again until it changes. -->
      <div class="actions">
        <button type="button" class="secondary" onclick={retry}>Try again</button>
      </div>
    {/if}
  {/if}
</div>
