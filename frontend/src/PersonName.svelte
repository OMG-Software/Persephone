<script lang="ts">
  // A person as the timeline shows them: their display name, then their
  // user ID in brackets in a softer type. Anyone can choose any display
  // name, so the ID always shows beside it. Both are untrusted text, each
  // isolated so that neither can reorder the other or the line around them.
  let { userId, name = null }: { userId: string; name?: string | null } = $props();
</script>

{#if name !== null}<bdi class="person-name">{name}</bdi>{' '}<span class="person-id"
    >(<bdi>{userId}</bdi>)</span
  >{:else}<bdi>{userId}</bdi>{/if}
