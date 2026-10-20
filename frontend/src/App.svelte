<script lang="ts">
  import { onMount } from 'svelte';
  import markUrl from './assets/mark.svg';
  import Message from './Message.svelte';
  import { appStatus, type AppStatus } from './bridge';

  const sample = {
    name: 'Example sender',
    body: 'The displayed text is untrusted data.'
  };

  let status: AppStatus | null = $state(null);
  let unavailable = $state(false);

  onMount(() => {
    appStatus()
      .then((reply) => (status = reply))
      .catch(() => (unavailable = true));
  });
</script>

<main>
  <header>
    <img src={markUrl} alt="" width="36" height="36" />
    <div>
      <h1>Persephone</h1>
      <p role="status">
        {#if status}
          Rust host {status.version} · {status.milestone}
        {:else if unavailable}
          Rust host unavailable
        {:else}
          Connecting to the Rust host…
        {/if}
      </p>
    </div>
  </header>

  <section aria-labelledby="timeline-heading">
    <h2 id="timeline-heading">Message rendering sample</h2>
    <Message name={sample.name} body={sample.body} />
  </section>
</main>
