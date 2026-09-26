<script lang="ts">
  import { formatElapsed } from '../time';

  let { at }: { at: number } = $props();

  let now = $state(Date.now());
  const left = $derived(at - now);
  const alive = $derived(left > 0);
  $effect(() => {
    if (!alive) return;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
</script>

<span class="hint" class:expired={!alive} data-testid="expiry">
  {alive ? `Expires in ${formatElapsed(left)}` : 'Expired. Create a new one.'}
</span>

<style>
  .expired {
    color: var(--failed);
  }
</style>
