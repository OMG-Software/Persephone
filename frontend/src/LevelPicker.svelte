<script lang="ts">
  import { levelChoices, parseLevel } from './roomSettings';

  // Picks a power level: a named role, or any whole number. A choice is
  // applied at once; a custom level only when the user sets it.
  let {
    id,
    label,
    level,
    own,
    disabled = false,
    onpick
  }: {
    id: string;
    // The accessible name; may hold untrusted text (a member's name).
    label: string;
    level: number;
    // This account's level; null for a room creator.
    own: number | null;
    disabled?: boolean;
    onpick: (level: number) => void;
  } = $props();

  const CUSTOM = 'custom';

  let custom = $state(false);
  let typed = $state('');

  const choices = $derived(levelChoices(level, own));
  const typedLevel = $derived(parseLevel(typed));

  function choose(event: Event & { currentTarget: HTMLSelectElement }) {
    const value = event.currentTarget.value;
    if (value === CUSTOM) {
      custom = true;
      typed = String(level);
      return;
    }
    custom = false;
    const picked = Number(value);
    if (picked !== level) onpick(picked);
  }

  function set(event: SubmitEvent) {
    event.preventDefault();
    if (typedLevel === null) return;
    custom = false;
    if (typedLevel !== level) onpick(typedLevel);
  }
</script>

<div class="level-picker">
  <select
    {id}
    class="field"
    aria-label={label}
    {disabled}
    value={custom ? CUSTOM : String(level)}
    onchange={choose}
  >
    {#each choices as choice (choice.level)}
      <option value={String(choice.level)}>{choice.label}</option>
    {/each}
    <option value={CUSTOM}>Custom…</option>
  </select>
  {#if custom}
    <form class="level-custom" onsubmit={set}>
      <input
        class="field"
        type="text"
        inputmode="numeric"
        aria-label="Custom level"
        maxlength="17"
        autocomplete="off"
        bind:value={typed}
      />
      <button type="submit" class="secondary" disabled={disabled || typedLevel === null}>Set</button>
      <button type="button" class="secondary" onclick={() => (custom = false)}>Cancel</button>
    </form>
  {/if}
</div>
