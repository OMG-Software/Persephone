<script lang="ts">
  import FeatureDialog from './FeatureDialog.svelte';
  import HealthDetails from './HealthDetails.svelte';
  let { onclose }: { onclose: () => void } = $props();
</script>
<FeatureDialog title="Connection health" {onclose}>
  <HealthDetails />
</FeatureDialog>
