<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import PaymentTrail from './components/PaymentTrail.svelte';
  import HcsTrail from './components/HcsTrail.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { historyState } from './historyState.svelte.js';
  import { onMount } from 'svelte';

  onMount(() => {
    if (!historyState.historyPaymentsLoadedOnce) {
      historyState.loadHistoryPayments();
    }
    if (!historyState.historyHcsLoadedOnce) {
      historyState.loadHistoryHcs();
    }
  });
</script>

<section class="chat-panel">
  <TopBar title="History">
    <button
      onclick={() => historyState.loadHistory()}
      disabled={!daemonState.online || historyState.historyPaymentsLoading || historyState.historyHcsLoading}
    >
      {(historyState.historyPaymentsLoading || historyState.historyHcsLoading) ? 'Refreshing…' : '↻ Refresh'}
    </button>
  </TopBar>

  <div class="history-panel">
    {#if !daemonState.online}
      <p>Daemon offline. Start the daemon to load history.</p>
    {:else}
      <PaymentTrail />
      <HcsTrail />
    {/if}
  </div>
</section>
