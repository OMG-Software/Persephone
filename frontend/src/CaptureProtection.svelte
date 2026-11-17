<script lang="ts">
  import type { AccountSettings, DisplaySession } from './bridge';
  import { captureSessionText } from './capture';
  import type { DesktopPlatform } from './modules';

  // The account's capture protection switch (ADR-0045). The desktop host
  // reads the saved value and applies it to the window. On Linux, which
  // cannot hide a window, a note says who can read it (ADR-0047).
  let {
    settings,
    failure,
    platform = 'windows',
    session = null,
    onchange
  }: {
    settings: AccountSettings;
    failure: string | null;
    platform?: DesktopPlatform;
    // The display server on Linux; null until the host answers.
    session?: DisplaySession | null;
    onchange: (patch: Partial<AccountSettings>) => void;
  } = $props();
</script>

<!-- A card of the Security page (App.svelte stacks it under the encryption cards). -->
<section class="panel" aria-labelledby="capture-heading">
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
  <p class="note switch-note">
    On by default. Screenshots, screen recordings and screen sharing show the window blank. Some
    tools that read the screen, such as magnifiers, may also show it blank. This does not stop
    other programs on this computer from reading Persephone in other ways. Windows only.
  </p>
  {#if platform === 'linux'}
    <p class="note" role="status">{captureSessionText(session)}</p>
  {/if}
  {#if failure}
    <p role="alert" class="error">{failure}</p>
  {/if}
</section>
