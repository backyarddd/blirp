<script lang="ts">
  import { untrack } from 'svelte';
  import type { Session } from '../api/types.gen';
  import { hasTerminal, sessionTitle } from '../status';
  import Terminal from '../terminal/Terminal.svelte';
  import SessionDetail from '../../screens/SessionDetail.svelte';

  let { session, autofocus = false }: { session: Session; autofocus?: boolean } = $props();

  // A terminal that was on screen stays (with its exit banner) after the process ends, until
  // the user asks for the details view or switches sessions. Resuming it remounts the pane
  // (`epoch`) so it attaches to the new process.
  let terminalFor: string | null = $state(null);
  let epoch = $state(0);
  let wasLive = false;
  $effect(() => {
    const id = session.id;
    const live = hasTerminal(session);
    untrack(() => {
      if (live) {
        if (!wasLive && terminalFor === id) epoch++;
        terminalFor = id;
      } else if (terminalFor !== id) {
        terminalFor = null;
      }
    });
    wasLive = live;
  });
  const showTerminal = $derived(hasTerminal(session) || terminalFor === session.id);
</script>

<div class="pane-body">
  {#if showTerminal}
    {#key `${session.id}:${epoch}`}
      <Terminal
        sessionId={session.id}
        label="Terminal: {sessionTitle(session)}"
        {autofocus}
        ondetails={() => (terminalFor = null)}
      />
    {/key}
  {:else}
    <SessionDetail {session} />
  {/if}
</div>

<style>
  .pane-body {
    height: 100%;
    position: relative;
    min-height: 0;
  }
</style>
