<script lang="ts">
  let { name, body }: { name: string; body: string } = $props();
</script>

<!-- Names are isolated with <bdi> and bodies with dir="auto", so bidi control
     characters in one message cannot reorder text outside it. All text is
     rendered as escaped text, never as HTML. -->
<article aria-label={'Message from ' + name}>
  <strong><bdi>{name}</bdi></strong>
  <p dir="auto">{body}</p>
</article>
