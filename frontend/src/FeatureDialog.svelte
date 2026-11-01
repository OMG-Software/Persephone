<script lang="ts">
  import { onMount, type Snippet } from 'svelte';
  let { title, onclose, children, closeDisabled = false }: {
    title: string; onclose: () => void; children: Snippet; closeDisabled?: boolean;
  } = $props();
  let dialog = $state<HTMLDialogElement | null>(null);
  onMount(/**
   * Open the modal and restore the previously focused control when it closes.
   */ () => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.showModal();
    return () => { dialog?.close(); previous?.focus(); };
  });
</script>

<dialog class="feature-dialog" aria-label={title} bind:this={dialog}
  oncancel={/**
    * Let the owner close the dialog only when no protected operation blocks dismissal.
    */ (event) => { event.preventDefault(); if (!closeDisabled) onclose(); }}>
  <header><h2>{title}</h2><button type="button" class="secondary" disabled={closeDisabled} onclick={onclose}>Close</button></header>
  {@render children()}
</dialog>
