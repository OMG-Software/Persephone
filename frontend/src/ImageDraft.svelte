<script lang="ts">
  import ImageCanvas from './ImageCanvas.svelte';
  import { formatSize, mimeText, type ImageDraft } from './image';

  // A picked image before it is sent. The preview is decoded from the copy
  // that will be sent, so it shows what the room will get.
  let {
    draft,
    sending,
    blocked,
    onsend,
    oncancel
  }: {
    draft: ImageDraft;
    sending: boolean;
    blocked: boolean;
    onsend: () => void;
    oncancel: () => void;
  } = $props();

  let failed = $state(false);
</script>

<div class="image-draft" role="group" aria-labelledby="image-draft-title">
  <p id="image-draft-title" class="image-draft-title">Image to send</p>
  {#if failed}
    <p class="error" role="alert">The preview could not be shown. Cancel, then attach the image again.</p>
  {:else}
    <div class="image-frame preview">
      <ImageCanvas view={draft.preview} label="Preview of the image to send" onerror={() => (failed = true)} />
    </div>
  {/if}
  <p class="note">
    {mimeText[draft.mime] ?? 'Image'} · {draft.preview.width} × {draft.preview.height} · {formatSize(draft.size)}.
    Location, camera and other details in the file are removed before it is sent.
  </p>
  <div class="actions">
    <button type="button" class="primary" disabled={sending || failed || blocked} onclick={onsend}>
      {sending ? 'Sending…' : 'Send image'}
    </button>
    <button type="button" class="secondary" disabled={sending} onclick={oncancel}>Cancel</button>
  </div>
</div>
