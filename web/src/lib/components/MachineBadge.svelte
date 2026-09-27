<script lang="ts">
  import Cloud from '@lucide/svelte/icons/cloud';
  import Monitor from '@lucide/svelte/icons/monitor';
  import Coffee from '@lucide/svelte/icons/coffee';
  import { app } from '../app.svelte';

  /**
   * Shows where a session runs. This machine's sessions are labeled too once other machines are
   * paired, so a mixed list never reads as if only the other machine's sessions were there.
   */
  let { machineId }: { machineId: string } = $props();

  const info = $derived(app.remote(machineId));
  const self = $derived(info === null && machineId === app.selfId && app.machines.length > 1);
  const awake = $derived(info !== null && app.awake[info.id] === true);
  const title = $derived(
    info === null
      ? ''
      : `${info.cloud ? `Cloud session: runs on your hub ${info.name}` : `Runs on ${info.name}`}${awake ? `, which is kept awake while it runs` : ''}`,
  );
</script>

{#if self}
  <span class="mbadge" title="Runs on this machine" data-testid="machine-badge">
    <Monitor size={12} aria-hidden="true" />
    <span class="ellipsis">{app.machineName(machineId)}</span>
  </span>
{:else if info}
  <span class="mbadge" class:cloud={info.cloud} {title} data-testid="machine-badge">
    {#if info.cloud}<Cloud size={12} aria-hidden="true" />{:else}<Monitor size={12} aria-hidden="true" />{/if}
    <span class="ellipsis">{info.name}</span>
    {#if awake}<Coffee size={11} aria-label="kept awake" />{/if}
  </span>
{/if}

<style>
  .mbadge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 150px;
    height: 20px;
    padding: 0 7px;
    border-radius: 999px;
    font-size: 11.5px;
    font-weight: 600;
    color: var(--text-2);
    background: var(--panel-2);
    border: 1px solid var(--border);
    white-space: nowrap;
    flex: none;
  }
  .mbadge.cloud {
    color: var(--accent);
    background: var(--accent-soft);
    border-color: color-mix(in srgb, var(--accent) 30%, var(--border));
  }
  .mbadge :global(svg) {
    flex: none;
  }
</style>
