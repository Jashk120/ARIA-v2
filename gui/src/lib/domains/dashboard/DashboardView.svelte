<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import BudgetSection from './components/BudgetSection.svelte';
  import WalletSection from './components/WalletSection.svelte';
  import HoldsSection from './components/HoldsSection.svelte';
  import AllowlistPreview from './components/AllowlistPreview.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { dashboardState } from './dashboardState.svelte.js';
  import { onMount } from 'svelte';

  onMount(() => {
    if (!dashboardState.dashboardLoadedOnce) {
      dashboardState.loadDashboard();
    }
  });

  const hasAnyData = $derived(
    dashboardState.dashboardBudget !== null ||
      dashboardState.dashboardHolds !== null ||
      dashboardState.dashboardAllowlist !== null ||
      dashboardState.dashboardWallet !== null
  );
</script>

<section class="chat-panel">
  <TopBar title="Dashboard">
    <button onclick={() => dashboardState.loadDashboard()} disabled={!daemonState.online || dashboardState.dashboardLoading}>
      {dashboardState.dashboardLoading ? 'Refreshing…' : '↻ Refresh'}
    </button>
  </TopBar>

  <div class="dashboard-panel">
    {#if !daemonState.online}
      <p>Daemon offline. Start the daemon to load dashboard data.</p>
    {:else if !dashboardState.dashboardLoadedOnce || (dashboardState.dashboardLoading && !hasAnyData)}
      <p>Loading…</p>
    {:else}
      <BudgetSection />
      <WalletSection />
      <HoldsSection />
      <AllowlistPreview />
    {/if}
  </div>
</section>
