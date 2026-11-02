<script lang="ts">
  import IconAction from './IconAction.svelte';

  // The actions on one timeline item, as icons. Each label shows as a
  // tooltip; screen readers hear the label with the sender, such as
  // "Reply to <name>". An action shows only when its handler is given.
  let {
    sender,
    reacting = false,
    onreact,
    onreply,
    onedit,
    ondelete,
    onpin,
    onreport
  }: {
    // Untrusted: rendered as isolated text only.
    sender: string;
    // Whether the quick reactions are open.
    reacting?: boolean;
    // Opens or closes the quick reactions.
    onreact?: () => void;
    onreply?: () => void;
    onedit?: () => void;
    ondelete?: () => void;
    onpin?: () => void;
    onreport?: () => void;
  } = $props();
</script>

{#if onreact || onreply || onedit || ondelete || onpin || onreport}
  <span class="message-actions">
    {#if onreact}
      <IconAction icon="smile" label="React" expanded={reacting} onclick={onreact}>
        to <bdi>{sender}</bdi>
      </IconAction>
    {/if}
    {#if onreply}
      <IconAction icon="reply" label="Reply" onclick={onreply}>to <bdi>{sender}</bdi></IconAction>
    {/if}
    {#if onedit}
      <IconAction icon="edit" label="Edit" onclick={onedit}>your message</IconAction>
    {/if}
    {#if ondelete}
      <IconAction icon="trash" label="Delete" onclick={ondelete}>your message</IconAction>
    {/if}
    {#if onpin}
      <IconAction icon="pin" label="Pin message" onclick={onpin}>from <bdi>{sender}</bdi></IconAction>
    {/if}
    {#if onreport}
      <IconAction icon="flag" label="Report message" onclick={onreport}>from <bdi>{sender}</bdi></IconAction>
    {/if}
  </span>
{/if}
