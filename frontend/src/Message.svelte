<script lang="ts">
  let { name, body }: { name: string; body: string } = $props();
</script>

<article aria-label={'Message from ' + name}>
  <strong>{name}</strong>
  <p>{body}</p>
</article>
