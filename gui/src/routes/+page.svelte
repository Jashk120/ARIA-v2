<script>
  import Sidebar from '$lib/domains/shell/Sidebar.svelte';
  import ChatView from '$lib/domains/chat/ChatView.svelte';
  import DirectView from '$lib/domains/direct/DirectView.svelte';
  import DashboardView from '$lib/domains/dashboard/DashboardView.svelte';
  import HistoryView from '$lib/domains/history/HistoryView.svelte';
  import ModelsView from '$lib/domains/models/ModelsView.svelte';
  import ForgeView from '$lib/domains/forge/ForgeView.svelte';
  import SettingsView from '$lib/domains/settings/SettingsView.svelte';
  import ContractsView from '$lib/domains/contracts/ContractsView.svelte';
  import TokensView from '$lib/domains/tokens/TokensView.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { chatState } from '$lib/domains/chat/chatState.svelte.js';
  import { navigationState } from '$lib/domains/shell/navigationState.svelte.js';
  import { onMount, onDestroy } from 'svelte';

  onMount(() => {
    daemonState.init();
    chatState.init();
  });

  onDestroy(() => {
    daemonState.destroy();
    chatState.destroy();
  });
</script>

<svelte:head><title>ARIA — AI Assistant</title></svelte:head>

<main class="shell">
  <Sidebar bind:activeTab={navigationState.tab} />

  {#if navigationState.tab === 'chat'}
    <ChatView />
  {:else if navigationState.tab === 'direct'}
    <DirectView />
  {:else if navigationState.tab === 'dashboard'}
    <DashboardView />
  {:else if navigationState.tab === 'history'}
    <HistoryView />
  {:else if navigationState.tab === 'models'}
    <ModelsView />
  {:else if navigationState.tab === 'forge'}
    <ForgeView />
  {:else if navigationState.tab === 'contracts'}
    <ContractsView />
  {:else if navigationState.tab === 'tokens'}
    <TokensView />
  {:else if navigationState.tab === 'settings'}
    <SettingsView />
  {/if}
</main>
