<script lang="ts">
  import { onMount, type Snippet } from 'svelte';
  let { title, onclose, children, closeDisabled = false }: {
    title: string; onclose: () => void; children: Snippet; closeDisabled?: boolean;
  } = $props();
  let dialog = $state<HTMLDialogElement | null>(null);
  /** Open the modal and restore the previously focused control when it closes. */
  function mountDialog() {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.showModal();
    return () => { dialog?.close(); previous?.focus(); };
  }
  onMount(mountDialog);

  /** Let the owner close the dialog only when no protected operation blocks dismissal. */
  function cancelDialog(event: Event) {
    event.preventDefault();
    if (!closeDisabled) onclose();
  }
</script>

<dialog class="feature-dialog" aria-label={title} bind:this={dialog}
  oncancel={cancelDialog}>
  <header><h2>{title}</h2><button type="button" class="secondary" disabled={closeDisabled} onclick={onclose}>Close</button></header>
  {@render children()}
</dialog>
