<script lang="ts">
  import type { ThirdPartyModules } from './bridge';
  import { processesLabel, signerLabel, stateText, unreviewed } from './modules';
  import { fullTime, toDate } from './time';

  // The code loaded into Persephone that Microsoft and Persephone did not
  // ship (Settings, Security). File names, paths and signers come from the
  // machine: isolated, escaped text only. A missing report shows "Checking…".
  let {
    report,
    busy = false,
    failure = null,
    onacknowledge
  }: {
    report: ThirdPartyModules | null;
    busy?: boolean;
    failure?: string | null;
    // The user marked every unreviewed module as reviewed.
    onacknowledge: () => void;
  } = $props();

  const text = $derived(stateText(report ?? { state: 'pending', modules: [] }));
  const anyNew = $derived(unreviewed(report).length > 0);

  function seen(millis: number): string {
    const date = toDate(millis);
    return date ? fullTime(date) : 'Unknown';
  }
</script>

<section class="settings-page loaded-code" aria-labelledby="loaded-code-heading">
  <h2 id="loaded-code-heading">Code loaded into Persephone</h2>
  <p class="lede">
    Windows and security software can load their own code into apps. Persephone blocks hook-based
    code and code that Microsoft did not sign. This list shows the other code that is loaded.
  </p>
  {#if text}
    <p class="note" role="status">{text}</p>
  {:else if report}
    <ul class="module-list">
      <!-- Keyed by position: names are the machine's word and can repeat. -->
      {#each report.modules as module, index (index)}
        <li class:new={!module.acknowledged}>
          <span class="module-name">
            <bdi>{module.fileName}</bdi>
            {#if !module.acknowledged}<strong class="module-new">New</strong>{/if}
          </span>
          <span class="module-signer"><bdi>{signerLabel(module)}</bdi></span>
          <span class="module-where">{processesLabel(module.processes)}</span>
          <span class="module-seen note">First seen {seen(module.firstSeen)}</span>
          <span class="module-path note"><bdi class="mono">{module.path}</bdi></span>
        </li>
      {/each}
    </ul>
    {#if report.truncated}
      <p class="note">More modules are loaded than this list shows.</p>
    {/if}
  {/if}
  {#if failure}
    <p class="error" role="alert">{failure}</p>
  {/if}
  {#if anyNew}
    <div class="actions">
      <button type="button" class="secondary" disabled={busy} onclick={onacknowledge}>
        Mark all as reviewed
      </button>
    </div>
  {/if}
</section>
