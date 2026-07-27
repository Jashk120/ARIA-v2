<script>
  import { daemonState } from '$lib/services/daemonState.svelte.js';

  let { title = 'ARIA', children = undefined } = $props();
</script>

<header class="top-bar">
  <div class="top-bar-left">
    <span class="top-bar-title">{title}</span>
  </div>
  <div class="top-bar-right">
    {#if children}
      {@render children()}
    {:else}
      <div class="status-pill" class:online={daemonState.online}>
        <span class="pulse"></span>
        {daemonState.online ? 'Connected' : 'No Daemon'}
      </div>
    {/if}
  </div>
</header>
