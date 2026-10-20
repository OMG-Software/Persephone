<script lang="ts">
  import markUrl from './assets/mark.svg';
  import Message from './Message.svelte';

  const sample = {
    name: 'Example sender',
    body: 'The displayed text is untrusted data.'
  };
</script>

<main>
  <header>
    <img src={markUrl} alt="" width="36" height="36" />
    <div>
      <h1>Persephone frontend probe</h1>
      <p>Local static UI · escaped message text</p>
    </div>
  </header>

  <section aria-labelledby="timeline-heading">
    <h2 id="timeline-heading">Message rendering sample</h2>
    <Message name={sample.name} body={sample.body} />
  </section>
</main>
