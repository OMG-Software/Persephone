<script lang="ts">
  import { onDestroy, untrack } from 'svelte';
  import type { CallVideoTile } from '../../crates/contracts/generated/contracts';
  import { callVideoFrame } from './bridge';
  import { OWN_TILE_LABEL, PARTICIPANT_TILE_LABEL } from './calls';
  import PersonName from './PersonName.svelte';
  import { tileLayout, VideoFetcher, type TileRef, type VideoPicture } from './video';

  // The video tiles of the call: the own camera (mirrored, labelled "You")
  // and up to four other devices. The core sends plain RGBA pictures; this
  // draws each on a canvas and does nothing else with them. Names are
  // untrusted text. `ticks` goes up each time the core says a new picture is
  // waiting. Unmounting stops every request.
  let {
    tiles,
    names,
    ticks
  }: {
    tiles: readonly CallVideoTile[];
    // Display names from the call room's members, by user ID.
    names: ReadonlyMap<string, string>;
    ticks: number;
  } = $props();

  type Shape = { width: number; height: number; rotation: number };

  const canvases: Record<number, HTMLCanvasElement | undefined> = {};
  let shapes = $state<Record<number, Shape | undefined>>({});

  function show(tile: number, picture: VideoPicture) {
    const canvas = canvases[tile];
    const context = canvas?.getContext('2d');
    if (!canvas || !context) return;
    if (canvas.width !== picture.width) canvas.width = picture.width;
    if (canvas.height !== picture.height) canvas.height = picture.height;
    context.putImageData(new ImageData(picture.pixels, picture.width, picture.height), 0, 0);
    const shape = shapes[tile];
    if (
      !shape ||
      shape.width !== picture.width ||
      shape.height !== picture.height ||
      shape.rotation !== picture.rotation
    ) {
      shapes[tile] = { width: picture.width, height: picture.height, rotation: picture.rotation };
    }
  }

  function clear(tile: number) {
    const canvas = canvases[tile];
    canvas?.getContext('2d')?.clearRect(0, 0, canvas.width, canvas.height);
    shapes[tile] = undefined;
  }

  const fetcher = new VideoFetcher(callVideoFrame, show, clear);

  const shown = $derived([...tiles].sort((a, b) => a.tile - b.tile));

  // The tile list changed: drop tiles that went, reset tiles another device
  // took or that show a new stream, and fetch the new ones.
  $effect(() => {
    const next: TileRef[] = shown.map((tile) => ({
      tile: tile.tile,
      key: tile.own ? 'own' : `${tile.participant?.userId}/${tile.participant?.deviceId}`,
      stream: tile.stream
    }));
    untrack(() => fetcher.setTiles(next));
  });

  // A new picture is waiting.
  $effect(() => {
    ticks;
    untrack(() => fetcher.poke());
  });

  onDestroy(() => fetcher.stop());

  function canvasRef(node: HTMLCanvasElement, tile: number) {
    canvases[tile] = node;
    return {
      destroy() {
        if (canvases[tile] === node) canvases[tile] = undefined;
      }
    };
  }
</script>

{#if shown.length > 0}
  <section class="call-video" aria-label="Call video">
    {#each shown as tile (tile.tile)}
      {@const shape = shapes[tile.tile]}
      {@const layout = shape ? tileLayout(shape.width, shape.height, shape.rotation, tile.own) : null}
      <figure class="video-tile" class:own={tile.own}>
        <div class="video-frame" style:aspect-ratio={layout ? String(layout.ratio) : '4 / 3'}>
          <canvas
            use:canvasRef={tile.tile}
            class="video-canvas"
            hidden={layout === null}
            aria-hidden="true"
            style:width={layout ? `${layout.canvasPercent}%` : undefined}
            style:transform={layout?.transform}
          ></canvas>
        </div>
        <figcaption class="video-name">
          {#if tile.own}
            {OWN_TILE_LABEL}
          {:else if tile.participant}
            <PersonName
              userId={tile.participant.userId}
              name={names.get(tile.participant.userId) ?? null}
            />
          {:else}
            {PARTICIPANT_TILE_LABEL}
          {/if}
        </figcaption>
      </figure>
    {/each}
  </section>
{/if}
