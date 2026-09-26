<script lang="ts">
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import Folder from '@lucide/svelte/icons/folder';
  import type { Project } from '../api/types';

  let { project }: { project: Pick<Project, 'is_git'> } = $props();
</script>

{#if project.is_git}
  <span class="badge"><GitBranch size={12} aria-hidden="true" />git</span>
{:else}
  <span class="badge"><Folder size={12} aria-hidden="true" />folder</span>
{/if}
