<script lang="ts">
  import type { SessionStatus } from '../api/types.gen';
  import { statusInfo } from '../status';

  let { status }: { status: SessionStatus } = $props();
  const info = $derived(statusInfo(status));
</script>

<span class="chip {info.tone}">
  <span class="dot" class:pulse={info.pulse} aria-hidden="true"></span>{info.label}
</span>

<style>
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 22px;
    padding: 0 8px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 600;
    white-space: nowrap;
    flex: none;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: currentColor;
  }
  .dot.pulse {
    animation: pulse 1.4s ease-in-out infinite;
  }
  .working {
    color: var(--working);
    background: var(--working-bg);
  }
  .idle {
    color: var(--idle);
    background: var(--idle-bg);
  }
  .waiting {
    color: var(--waiting);
    background: var(--waiting-bg);
  }
  .completed {
    color: var(--completed);
    background: var(--completed-bg);
  }
  .failed {
    color: var(--failed);
    background: var(--failed-bg);
  }
  .detached {
    color: var(--detached);
    background: var(--detached-bg);
    border: 1px dashed var(--border-strong);
  }
</style>
