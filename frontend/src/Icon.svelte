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
    | 'shield-off'
    | 'chevron-left'
    | 'chevron-right'
    | 'chevron-up'
    | 'globe'
    | 'phone'
    | 'mic'
    | 'mic-off'
    | 'headphones'
    | 'headphones-off'
    | 'volume'
    | 'video'
    | 'video-off'
    | 'screen';
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
  {:else if name === 'globe'}
    <circle cx="12" cy="12" r="9" />
    <path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18" />
  {:else if name === 'phone'}
    <path d="M5 4h4l2 5-2.5 1.5a11 11 0 0 0 5 5L15 13l5 2v4a2 2 0 0 1-2 2A16 16 0 0 1 3 6a2 2 0 0 1 2-2Z" />
  {:else if name === 'mic'}
    <rect x="9" y="3" width="6" height="11" rx="3" />
    <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
  {:else if name === 'mic-off'}
    <path d="M9 9v2a3 3 0 0 0 5 2.2M15 9.3V6a3 3 0 0 0-5.9-.7M5 11a7 7 0 0 0 11.3 5.5M19 11a7 7 0 0 1-.6 2.8M12 18v3M3 3l18 18" />
  {:else if name === 'headphones'}
    <path d="M3 18v-6a9 9 0 0 1 18 0v6" />
    <path d="M21 19a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3ZM3 19a2 2 0 0 0 2 2h1a2 2 0 0 0 2-2v-3a2 2 0 0 0-2-2H3Z" />
  {:else if name === 'headphones-off'}
    <path d="M3 18v-6a9 9 0 0 1 15.4-6.4M21 12v6" />
    <path d="M21 19a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3ZM3 19a2 2 0 0 0 2 2h1a2 2 0 0 0 2-2v-3a2 2 0 0 0-2-2H3ZM3 3l18 18" />
  {:else if name === 'volume'}
    <path d="M11 5 6 9H2v6h4l5 4V5Z" />
    <path d="M15.5 8.5a5 5 0 0 1 0 7M19 5a10 10 0 0 1 0 14" />
  {:else if name === 'video'}
    <rect x="3" y="6" width="12" height="12" rx="2" />
    <path d="m15 10 6-3v10l-6-3" />
  {:else if name === 'screen'}
    <rect x="3" y="4" width="18" height="12" rx="2" />
    <path d="M8 20h8M12 16v4" />
  {:else if name === 'video-off'}
    <path d="M10 6h3a2 2 0 0 1 2 2v3l6-4v10M15 15v1a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2M3 3l18 18" />
  {:else if name === 'chevron-left'}
    <path d="m15 6-6 6 6 6" />
  {:else if name === 'chevron-right'}
    <path d="m9 6 6 6-6 6" />
  {:else if name === 'chevron-up'}
    <path d="m6 15 6-6 6 6" />
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
