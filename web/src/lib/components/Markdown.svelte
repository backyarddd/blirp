<script lang="ts">
  import { renderMarkdown } from '../markdown';

  let { source, class: cls = '' }: { source: string; class?: string } = $props();
  const html = $derived(renderMarkdown(source));
</script>

<!-- renderMarkdown sanitizes with DOMPurify -->
<div class="md {cls}">{@html html}</div>
