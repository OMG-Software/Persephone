<script lang="ts">
  import type { CallVideoEffect } from '../../crates/contracts/generated/contracts';
  import {
    BACKGROUND_EFFECTS_UNAVAILABLE_TEXT,
    BACKGROUND_LABEL,
    BLUR_LABEL,
    BLUR_MAX,
    BLUR_MIN,
    BLUR_STEP,
    FILTER_LABEL,
    MIRROR_HELP,
    MIRROR_LABEL,
    VIDEO_EFFECTS_HELP,
    VIDEO_EFFECTS_TITLE,
    backgroundOptions,
    backgroundValue,
    blurText,
    blurValue,
    filterOptions,
    filterValue,
    type VideoEffectPart
  } from './calls';

  // The own camera's video effect and the preview's mirror (ADR-0068), for
  // the main window's call settings and the call window's device panel. The
  // values are a closed set; the host and the core check them again, and
  // the worker refuses an effect that cannot run. Blur and the pictures need
  // the person mask: without it (`available` false) they cannot be chosen.
  // `onsave` runs after each change the user makes: a menu choice, a
  // released slider, a switched box, with the part it changed (the effect
  // or the mirror). The parent sends only that part and shows the result.
  let {
    effect = $bindable(),
    mirror = $bindable(),
    available,
    disabled,
    prefix,
    heading = 'h4',
    onsave
  }: {
    effect: CallVideoEffect;
    mirror: boolean;
    available: boolean;
    disabled: boolean;
    // Makes the control IDs unique on the page.
    prefix: string;
    heading?: 'h3' | 'h4';
    onsave: (part: VideoEffectPart) => void;
  } = $props();

  const backgrounds = $derived(backgroundOptions(available, effect.background));
  const filters = filterOptions();
</script>

<svelte:element this={heading} class="device-tests-title">{VIDEO_EFFECTS_TITLE}</svelte:element>
<label for="{prefix}-background">{BACKGROUND_LABEL}</label>
<select
  id="{prefix}-background"
  class="field"
  {disabled}
  value={effect.background}
  onchange={(event) => {
    effect = { ...effect, background: backgroundValue(event.currentTarget.value, effect.background) };
    onsave('effect');
  }}
>
  {#each backgrounds as option (option.value)}
    <option value={option.value} disabled={option.disabled}>{option.label}</option>
  {/each}
</select>
{#if !available}
  <p class="note">{BACKGROUND_EFFECTS_UNAVAILABLE_TEXT}</p>
{/if}
{#if effect.background === 'blur'}
  <label for="{prefix}-blur">{BLUR_LABEL}: <output for="{prefix}-blur">{blurText(effect.blur)}</output></label>
  <input
    id="{prefix}-blur"
    type="range"
    min={BLUR_MIN}
    max={BLUR_MAX}
    step={BLUR_STEP}
    {disabled}
    value={effect.blur}
    aria-valuetext={blurText(effect.blur)}
    oninput={(event) => (effect = { ...effect, blur: blurValue(Number(event.currentTarget.value)) })}
    onchange={() => onsave('effect')}
  />
{/if}
<label for="{prefix}-filter">{FILTER_LABEL}</label>
<select
  id="{prefix}-filter"
  class="field"
  {disabled}
  value={effect.filter}
  onchange={(event) => {
    effect = { ...effect, filter: filterValue(event.currentTarget.value, effect.filter) };
    onsave('effect');
  }}
>
  {#each filters as option (option.value)}
    <option value={option.value}>{option.label}</option>
  {/each}
</select>
<p class="note">{VIDEO_EFFECTS_HELP}</p>
<label class="switch-row">
  <input
    type="checkbox"
    role="switch"
    {disabled}
    checked={mirror}
    onchange={(event) => {
      mirror = event.currentTarget.checked;
      onsave('mirror');
    }}
  />
  <span>{MIRROR_LABEL}</span>
</label>
<p class="note switch-note">{MIRROR_HELP}</p>
