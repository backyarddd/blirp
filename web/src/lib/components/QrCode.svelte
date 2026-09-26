<script lang="ts">
  import { toCanvas } from 'qrcode';

  let { text, label, size = 200 }: { text: string; label: string; size?: number } = $props();
  let canvas: HTMLCanvasElement | undefined = $state();
  let error: string | null = $state(null);

  $effect(() => {
    if (!canvas) return;
    // Always dark-on-white: inverted codes scan poorly on many phones.
    toCanvas(canvas, text, { width: size, margin: 2, errorCorrectionLevel: 'M' })
      .then(() => (error = null))
      .catch((e: unknown) => (error = e instanceof Error ? e.message : String(e)));
  });
</script>

{#if error}
  <p class="err" role="alert">Could not render QR code: {error}</p>
{/if}
<div role="img" aria-label={label} hidden={error !== null}><canvas bind:this={canvas} class="qr"></canvas></div>

<style>
  .qr {
    border-radius: 8px;
    background: #fff;
    border: 1px solid var(--border);
  }
  .err {
    color: var(--danger);
  }
</style>
