<script lang="ts">
  import type { AccountSettings } from './bridge';

  // The account's capture protection switch (ADR-0045). The desktop host
  // reads the saved value and applies it to the window.
  let {
    settings,
    failure,
    onchange
  }: {
    settings: AccountSettings;
    failure: string | null;
    onchange: (patch: Partial<AccountSettings>) => void;
  } = $props();
</script>

<section class="settings-page" aria-labelledby="capture-heading">
  <div class="panel">
    <h3 id="capture-heading">Screen capture</h3>
    <label class="switch-row">
      <input
        type="checkbox"
        role="switch"
        checked={settings.captureProtection}
        onchange={(event) => onchange({ captureProtection: event.currentTarget.checked })}
      />
      <span>Hide Persephone from screenshots and screen sharing</span>
    </label>
    <p class="note">
      On by default. Screenshots, screen recordings and screen sharing show the window blank. Some
      tools that read the screen, such as magnifiers, may also show it blank. This does not stop
      other programs on this computer from reading Persephone in other ways. Windows only.
    </p>
    {#if failure}
      <p role="alert" class="error">{failure}</p>
    {/if}
  </div>
</section>
