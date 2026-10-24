<script lang="ts">
  import { onMount } from 'svelte';
  import { imageBand, type ImageView } from './bridge';
  import { BAD_IMAGE, assembleImage } from './image';

  // Draws the pixels the core holds under `view.handle`, once all bands have
  // arrived and fit together: nothing partial is drawn. It does not release
  // the handle; the owner does. Mount it again (with {#key}) for another view.
  let {
    view,
    label,
    ondone = () => {},
    onerror = () => {}
  }: {
    view: ImageView;
    // The accessible name, from untrusted text: isolate it before passing it.
    label: string;
    ondone?: () => void;
    onerror?: (reason: unknown) => void;
  } = $props();

  let canvas = $state<HTMLCanvasElement | null>(null);

  onMount(() => {
    let stopped = false;
    const shown = view;
    (async () => {
      try {
        const pixels = await assembleImage(shown, (index) => imageBand(shown.handle, index), () => stopped);
        if (pixels === null || stopped) return;
        const context = canvas?.getContext('2d');
        if (!context) throw BAD_IMAGE;
        context.putImageData(new ImageData(pixels, shown.width, shown.height), 0, 0);
        ondone();
      } catch (reason) {
        if (!stopped) onerror(reason);
      }
    })();
    return () => {
      stopped = true;
    };
  });
</script>

<!-- The CSS scales the canvas to fit; its width and height attributes keep
     the aspect ratio. -->
<div class="image-view" role="img" aria-label={label}>
  <canvas class="image-canvas" bind:this={canvas} width={view.width} height={view.height} aria-hidden="true"
  ></canvas>
</div>
