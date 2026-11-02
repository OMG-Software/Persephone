<script lang="ts" module>
  export type IconName =
    | 'search'
    | 'hash'
    | 'settings'
    | 'lock'
    | 'send'
    | 'back'
    | 'shield'
    | 'image'
    | 'plus'
    | 'users'
    | 'close'
    | 'paperclip'
    | 'star'
    | 'menu'
    | 'more'
    | 'smile'
    | 'reply'
    | 'edit'
    | 'trash'
    | 'pin'
    | 'flag'
    | 'verify'
    | 'shield-alert'
    | 'shield-off';
</script>

<script lang="ts">
  // Stroke icons on a 24-unit grid. Decorative: the control that holds one
  // carries the accessible name.
  let {
    name,
    class: extra = ''
  }: {
    name: IconName;
    class?: string;
  } = $props();
</script>

<svg class="icon {extra}" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
  {#if name === 'search'}
    <circle cx="11" cy="11" r="7" />
    <path d="m20 20-3.5-3.5" />
  {:else if name === 'hash'}
    <path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" />
  {:else if name === 'settings'}
    <circle cx="12" cy="12" r="3" />
    <path
      d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z"
    />
  {:else if name === 'lock'}
    <rect x="5" y="11" width="14" height="10" rx="2" />
    <path d="M8 11V7a4 4 0 0 1 8 0v4" />
  {:else if name === 'send'}
    <path d="M5 12h14M13 6l6 6-6 6" />
  {:else if name === 'back'}
    <path d="M19 12H5M11 18l-6-6 6-6" />
  {:else if name === 'shield'}
    <path d="M12 3 4 6v6c0 4.5 3.4 8 8 9 4.6-1 8-4.5 8-9V6Z" />
    <path d="m9 12 2 2 4-4" />
  {:else if name === 'image'}
    <rect x="3" y="5" width="18" height="14" rx="2" />
    <circle cx="9" cy="10" r="1.5" />
    <path d="m21 16-5-5-9 8" />
  {:else if name === 'paperclip'}
    <path d="m21 11-8.5 8.5a5 5 0 0 1-7-7L14 4a3.3 3.3 0 0 1 4.7 4.7l-8.5 8.5a1.7 1.7 0 0 1-2.4-2.4L15.5 7" />
  {:else if name === 'plus'}
    <path d="M12 5v14M5 12h14" />
  {:else if name === 'users'}
    <circle cx="9" cy="8" r="3.5" />
    <path d="M2.5 20a6.5 6.5 0 0 1 13 0M16 4.5a3.5 3.5 0 0 1 0 7M18 14.5a6.5 6.5 0 0 1 3.5 5.5" />
  {:else if name === 'close'}
    <path d="M6 6l12 12M18 6 6 18" />
  {:else if name === 'star'}
    <path d="m12 3 2.8 5.7 6.2.9-4.5 4.4 1.1 6.2L12 17.3l-5.6 2.9 1.1-6.2L3 9.6l6.2-.9Z" />
  {:else if name === 'menu'}
    <path d="M4 6h16M4 12h16M4 18h16" />
  {:else if name === 'more'}
    <path d="M12 5h.01M12 12h.01M12 19h.01" stroke-width="3" />
  {:else if name === 'smile'}
    <circle cx="12" cy="12" r="9" />
    <path d="M8 14.5a5 5 0 0 0 8 0M9 9.5h.01M15 9.5h.01" />
  {:else if name === 'reply'}
    <path d="M9 14 4 9l5-5" />
    <path d="M4 9h10a6 6 0 0 1 6 6v4" />
  {:else if name === 'edit'}
    <path d="M12 20h9" />
    <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />
  {:else if name === 'trash'}
    <path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6" />
  {:else if name === 'pin'}
    <path d="M12 17v5M8 3h8M9 3v7l-3 4v3h12v-3l-3-4V3" />
  {:else if name === 'flag'}
    <path d="M5 21V4M5 4h12l-2 4.5L17 13H5" />
  {:else if name === 'verify'}
    <path d="M12 3 4 6v6c0 4.5 3.4 8 8 9 4.6-1 8-4.5 8-9V6Z" />
    <path d="M9.8 9.5a2.3 2.3 0 0 1 4.4.8c0 1.5-2.2 1.9-2.2 3.2M12 16.5h.01" />
  {:else if name === 'shield-alert'}
    <path d="M12 3 4 6v6c0 4.5 3.4 8 8 9 4.6-1 8-4.5 8-9V6Z" />
    <path d="M12 8v4.5M12 16h.01" />
  {:else if name === 'shield-off'}
    <path d="M12 3 4 6v6c0 4.5 3.4 8 8 9 4.6-1 8-4.5 8-9V6Z" />
    <path d="M9 12h6" />
  {/if}
</svg>
