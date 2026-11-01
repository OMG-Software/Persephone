<script lang="ts">
  import type { LocalSearchHit } from './bridge';
  let { hit, onselect }: { hit: LocalSearchHit; onselect: (hit: LocalSearchHit) => void } = $props();
</script>

<button type="button" class="search-result" onclick={() => onselect(hit)}>
  <span>Room: <bdi>{hit.roomId}</bdi></span>
  <span>Sender: <bdi>{hit.sender}</bdi></span>
  <span dir="auto">{hit.body}</span>
  <span>Event: <bdi>{hit.eventId}</bdi></span>
</button>
