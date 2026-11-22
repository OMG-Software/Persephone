<script lang="ts" module>
  /** One command in a menu. Labels must be unique within one menu. */
  export type MenuItem = {
    label: string;
    onselect: () => void;
    disabled?: boolean;
    // Draws a line above the item, to group commands.
    separated?: boolean;
    // Makes the item one choice of several (a menu item radio): whether it
    // is the current one.
    checked?: boolean;
    // A class for a mark drawn before the label, such as a presence dot.
    mark?: string;
  };
</script>

<script lang="ts">
  import { tick, type Snippet } from 'svelte';
  import Icon, { type IconName } from './Icon.svelte';
  import { menuTarget } from './menu.ts';

  // A menu button (WAI-ARIA menu button pattern). The trigger shows only an
  // icon; its label shows as a tooltip and is its accessible name. Arrow
  // keys, Home and End move between commands; Escape closes the menu and
  // returns focus to the trigger; Tab or a click outside closes it. With a
  // `trigger` snippet the trigger shows that content instead, and no tip.
  let {
    id,
    label,
    icon = 'more',
    items,
    align = 'end',
    side = 'below',
    tip = 'below',
    trigger: content
  }: {
    // Unique in the page: names the list and links it to the trigger.
    id: string;
    label: string;
    icon?: IconName;
    items: MenuItem[];
    // Which edge of the trigger the list lines up with.
    align?: 'start' | 'end';
    // Whether the list opens below the trigger or above it.
    side?: 'below' | 'above';
    tip?: 'above' | 'below' | 'start';
    // Visible trigger content; `label` stays its accessible name.
    trigger?: Snippet;
  } = $props();

  let open = $state(false);
  let dismissed = $state(false);
  let root = $state<HTMLElement | null>(null);
  let trigger = $state<HTMLButtonElement | null>(null);
  let list = $state<HTMLElement | null>(null);

  const enabled = () => items.map((item) => !item.disabled);
  const commands = () =>
    list ? [...list.querySelectorAll<HTMLButtonElement>('[role="menuitem"], [role="menuitemradio"]')] : [];

  /** Open the list and focus its first or last enabled command. */
  async function show(key: 'Home' | 'End') {
    open = true;
    await tick();
    const index = menuTarget(key, -1, enabled());
    if (index !== null) commands()[index]?.focus();
  }

  /** Close the list; return focus to the trigger when the user did not move it elsewhere. */
  function hide(restore: boolean) {
    open = false;
    if (restore) trigger?.focus();
  }

  /** Close first, so a dialog the command opens returns focus to the trigger. */
  function choose(item: MenuItem) {
    if (item.disabled) return;
    hide(true);
    item.onselect();
  }

  function triggerKey(event: KeyboardEvent) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      void show(event.key === 'ArrowDown' ? 'Home' : 'End');
    } else if (event.key === 'Escape' && open) {
      // Focus stays on the trigger when no command can take it.
      event.preventDefault();
      event.stopPropagation();
      hide(true);
    } else if (event.key === 'Escape' && !open && !dismissed) {
      dismissed = true;
      event.stopPropagation();
    }
  }

  function commandKey(event: KeyboardEvent, index: number) {
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      hide(true);
      return;
    }
    const next = menuTarget(event.key, index, enabled());
    if (next !== null) {
      event.preventDefault();
      commands()[next]?.focus();
    }
  }

  /** Close when focus leaves the menu, such as by Tab. */
  function focusLeft(event: FocusEvent) {
    const to = event.relatedTarget;
    if (open && !(to instanceof Node && root?.contains(to))) open = false;
  }

  /** Close on a press outside the menu. */
  function pressed(event: PointerEvent) {
    if (open && !(event.target instanceof Node && root?.contains(event.target))) open = false;
  }
</script>

<svelte:window onpointerdown={pressed} />

<div class="menu action-anchor tip-{tip}" bind:this={root} onfocusout={focusLeft}>
  <button
    bind:this={trigger}
    id={id + '-button'}
    type="button"
    class={content ? 'text-menu-trigger menu-trigger' : 'icon-button menu-trigger'}
    aria-label={content ? label : undefined}
    aria-haspopup="menu"
    aria-expanded={open}
    aria-controls={open ? id : undefined}
    onclick={() => (open ? hide(false) : void show('Home'))}
    onkeydown={triggerKey}
    onblur={() => (dismissed = false)}
    onmouseleave={() => (dismissed = false)}
  >
    {#if content}
      {@render content()}
    {:else}
      <Icon name={icon} />
      <span class="visually-hidden">{label}</span>
    {/if}
  </button>
  {#if !content}
    <span class="action-tip" class:dismissed={dismissed || open} aria-hidden="true">{label}</span>
  {/if}
  {#if open}
    <ul
      {id}
      class="menu-list align-{align} side-{side}"
      role="menu"
      aria-labelledby={id + '-button'}
      bind:this={list}
    >
      {#each items as item, index (item.label)}
        <li role="none" class:separated={item.separated}>
          <button
            type="button"
            role={item.checked === undefined ? 'menuitem' : 'menuitemradio'}
            aria-checked={item.checked}
            tabindex="-1"
            disabled={item.disabled}
            onclick={() => choose(item)}
            onkeydown={(event) => commandKey(event, index)}
            >{#if item.mark}<span class="menu-mark {item.mark}" aria-hidden="true"></span>{/if}{item.label}</button
          >
        </li>
      {/each}
    </ul>
  {/if}
</div>
