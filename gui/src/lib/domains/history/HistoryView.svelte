<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import PaymentTrail from './components/PaymentTrail.svelte';
  import HcsTrail from './components/HcsTrail.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { historyState } from './historyState.svelte.js';
  import { onMount } from 'svelte';

  /** @type {'payments' | 'hcs'} */
  let activeTrail = $state('payments');

  onMount(() => {
    if (!historyState.historyPaymentsLoadedOnce) {
      historyState.loadHistoryPayments();
    }
    if (!historyState.historyHcsLoadedOnce) {
      historyState.loadHistoryHcs();
    }
  });

  function handleRefresh() {
    if (activeTrail === 'payments') {
      historyState.loadHistoryPayments();
    } else {
      historyState.loadHistoryHcs();
    }
  }

  const isLoading = $derived(
    activeTrail === 'payments'
      ? historyState.historyPaymentsLoading
      : historyState.historyHcsLoading
  );
</script>

<section class="chat-panel">
  <TopBar title="History">
    <button
      onclick={handleRefresh}
      disabled={!daemonState.online || isLoading}
    >
      {isLoading ? 'Refreshing…' : '↻ Refresh'}
    </button>
  </TopBar>

  {#if !daemonState.online}
    <div class="history-offline">
      <span class="history-offline-icon">⚡</span>
      <p>Daemon offline. Start the daemon to load history.</p>
    </div>
  {:else}
    <div class="history-tabs">
      <button
        class="history-tab-btn {activeTrail === 'payments' ? 'history-tab-active' : ''}"
        onclick={() => (activeTrail = 'payments')}
      >
        <span class="history-tab-icon">💳</span>
        Payment Trail
        {#if historyState.historyPayments !== null}
          <span class="history-tab-count">{historyState.historyPayments.length}</span>
        {/if}
      </button>
      <button
        class="history-tab-btn {activeTrail === 'hcs' ? 'history-tab-active' : ''}"
        onclick={() => (activeTrail = 'hcs')}
      >
        <span class="history-tab-icon">🔗</span>
        HCS Decision Trail
        {#if historyState.historyHcs !== null}
          <span class="history-tab-count">{historyState.historyHcs.length}</span>
        {/if}
      </button>
    </div>

    <div class="history-trail-panel">
      {#if activeTrail === 'payments'}
        <PaymentTrail />
      {:else}
        <HcsTrail />
      {/if}
    </div>
  {/if}
</section>
