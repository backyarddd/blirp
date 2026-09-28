<script lang="ts">
  import { tick } from 'svelte';
  import { registerContextMenu } from '../contextmenu';
  import type { MenuItem } from '../menu';
  import Menu from './Menu.svelte';

  // The one menu every `use:contextmenu` item opens.
  let items: MenuItem[] = $state.raw([]);
  let label = $state('Actions');
  let menu: Menu | undefined = $state();

  $effect(() =>
    registerContextMenu((next, name, x, y, from) => {
      items = next;
      label = name;
      // Rendered with the new items before it is measured and shown.
      void tick().then(() => menu?.openAt(x, y, from));
    }),
  );
</script>

<Menu bind:this={menu} {items} {label} />
