<script lang="ts">
  import type { CallVideoQuality } from '../../crates/contracts/generated/contracts';
  import { VIDEO_QUALITY_HELP, VIDEO_QUALITY_LABEL, VIDEO_QUALITY_OPTIONS, videoQualityValue } from './calls';

  // The own camera's send resolution (ADR-0070), for the main window's call
  // settings and the call window's device panel. The choices are a closed
  // set; the host and the core check them again. `onsave` runs after each
  // change the user makes; the parent sends it and shows the result.
  let {
    quality = $bindable(),
    disabled,
    prefix,
    onsave
  }: {
    quality: CallVideoQuality;
    disabled: boolean;
    // Makes the control IDs unique on the page.
    prefix: string;
    onsave: (quality: CallVideoQuality) => void;
  } = $props();
</script>

<label for="{prefix}-quality">{VIDEO_QUALITY_LABEL}</label>
<select
  id="{prefix}-quality"
  class="field"
  {disabled}
  aria-describedby="{prefix}-quality-help"
  bind:value={quality}
  onchange={(event) => onsave(videoQualityValue(event.currentTarget.value))}
>
  {#each VIDEO_QUALITY_OPTIONS as option (option.value)}
    <option value={option.value}>{option.label}</option>
  {/each}
</select>
<p id="{prefix}-quality-help" class="note">{VIDEO_QUALITY_HELP}</p>
