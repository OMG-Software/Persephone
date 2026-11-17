<script lang="ts">
  import type { ThirdPartyModules } from './bridge';
  import {
    INCOMPLETE_TEXT,
    LEDE_TEXT,
    listsReasons,
    processesLabel,
    reasonLabel,
    stateText,
    unreviewed,
    type DesktopPlatform
  } from './modules';
  import { fullTime, shortDate, toDate } from './time';

  // The code loaded into Persephone that the system and Persephone did not
  // ship (Settings, Security). File names, paths and signers come from the
  // machine: isolated, escaped text only. A missing report shows "Checking…".
  let {
    report,
    platform = 'windows',
    busy = false,
    failure = null,
    onacknowledge
  }: {
    report: ThirdPartyModules | null;
    platform?: DesktopPlatform;
    busy?: boolean;
    failure?: string | null;
    // The user marked every unreviewed module as reviewed.
    onacknowledge: () => void;
  } = $props();

  const text = $derived(stateText(report ?? { state: 'pending', modules: [], incomplete: false }));
  const anyNew = $derived(unreviewed(report).length > 0);
  // Linux has no signatures: its rows say why they are listed instead.
  const whyHeading = $derived(listsReasons(report?.modules ?? []) ? 'Why listed' : 'Signed by');

  // The day in the column; the full time on hover. A time JavaScript cannot
  // show reads "Unknown".
  function seen(millis: number): { short: string; full: string; iso: string | undefined } {
    const date = toDate(millis);
    return date
      ? { short: shortDate(date), full: fullTime(date), iso: date.toISOString() }
      : { short: 'Unknown', full: 'Unknown', iso: undefined };
  }
</script>

<!-- A card of the Security page (App.svelte stacks it under the encryption cards). -->
<section class="panel loaded-code" aria-labelledby="loaded-code-heading">
  <div class="panel-head">
    <h3 id="loaded-code-heading">Code loaded into Persephone</h3>
    {#if anyNew}
      <button type="button" class="secondary" disabled={busy} onclick={onacknowledge}>
        Mark all as reviewed
      </button>
    {/if}
  </div>
  <p>{LEDE_TEXT[platform]}</p>
  {#if text}
    <p class="note" role="status">{text}</p>
  {:else if report}
    <table class="module-table">
      <thead>
        <tr>
          <th scope="col" class="col-file">File</th>
          <th scope="col" class="col-signer">{whyHeading}</th>
          <th scope="col" class="col-where">Loaded in</th>
          <th scope="col" class="col-seen">First seen</th>
        </tr>
      </thead>
      <tbody>
        <!-- Keyed by position: names are the machine's word and can repeat. -->
        {#each report.modules as module, index (index)}
          {@const when = seen(module.firstSeen)}
          <tr class:new={!module.acknowledged}>
            <td class="module-file">
              <span class="module-name">
                <bdi>{module.fileName}</bdi>
                {#if !module.acknowledged}<strong class="module-new">New</strong>{/if}
              </span>
              <bdi class="module-path mono">{module.path}</bdi>
            </td>
            <td class="module-signer" class:warn={module.signature !== 'valid'} data-label={whyHeading}>
              <bdi>{reasonLabel(module)}</bdi>
            </td>
            <td data-label="Loaded in">{processesLabel(module.processes)}</td>
            <td class="module-seen" data-label="First seen">
              <time datetime={when.iso} title={when.full}>{when.short}</time>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if report.truncated}
      <p class="note">More modules are loaded than this list shows.</p>
    {/if}
    {#if report.incomplete}
      <p class="note">{INCOMPLETE_TEXT}</p>
    {/if}
  {/if}
  {#if failure}
    <p class="error" role="alert">{failure}</p>
  {/if}
</section>
