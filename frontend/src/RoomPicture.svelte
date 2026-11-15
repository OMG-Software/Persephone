<script lang="ts">
  import type { Snippet } from 'svelte';
  import IconAction from './IconAction.svelte';
  import { pictureFailureText, type PictureState, type RoomPictures } from './roomPictures';

  // A room's picture (ADR-0041), drawn from the pixels `pictures` keeps once
  // it is loaded; until then `children` shows (the room's usual icon). It
  // loads nothing by itself. With `onload`, a room that has a picture not
  // loaded yet also shows a button that asks for it. The picture is
  // decorative: the room's name is always shown beside it.
  let {
    pictures = null,
    roomId,
    pictureId,
    size = 'small',
    onload,
    children
  }: {
    pictures?: RoomPictures | null;
    roomId: string;
    pictureId: string | null;
    size?: 'small' | 'large';
    onload?: () => void;
    children?: Snippet;
  } = $props();

  let version = $state(0);
  $effect(() => {
    if (!pictures) return;
    return pictures.subscribe(() => (version += 1));
  });

  const picture: PictureState = $derived.by(() => {
    void version;
    return pictures ? pictures.state(roomId, pictureId) : { kind: 'none' };
  });
  const askable = $derived(
    onload !== undefined &&
      pictureId !== null &&
      (picture.kind === 'none' || (picture.kind === 'failed' && picture.retry))
  );

  let canvas = $state<HTMLCanvasElement | null>(null);
  $effect(() => {
    if (picture.kind !== 'shown' || !canvas) return;
    canvas.getContext('2d')?.putImageData(new ImageData(picture.pixels, picture.width, picture.height), 0, 0);
  });
</script>

{#if picture.kind === 'shown'}
  <canvas
    class="room-picture {size}"
    bind:this={canvas}
    width={picture.width}
    height={picture.height}
    aria-hidden="true"
  ></canvas>
{:else}
  {@render children?.()}
  {#if askable && onload}
    <IconAction
      icon="image"
      class="picture-load"
      label={picture.kind === 'failed' ? 'Try loading the room picture again' : 'Load the room picture'}
      tip="below"
      onclick={onload}
    >
      {#if picture.kind === 'failed'}: {pictureFailureText(picture.reason)}{/if}
    </IconAction>
  {/if}
{/if}
