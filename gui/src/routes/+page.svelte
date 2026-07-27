<script>
  import Sidebar from '$lib/domains/shell/Sidebar.svelte';
  import ChatView from '$lib/domains/chat/ChatView.svelte';
  import DirectView from '$lib/domains/direct/DirectView.svelte';
  import DashboardView from '$lib/domains/dashboard/DashboardView.svelte';
  import HistoryView from '$lib/domains/history/HistoryView.svelte';
  import SettingsView from '$lib/domains/settings/SettingsView.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { chatState } from '$lib/domains/chat/chatState.svelte.js';
  import { onMount, onDestroy } from 'svelte';

  let activeTab = $state('chat');

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
  <Sidebar bind:activeTab />

  {#if activeTab === 'chat'}
    <ChatView />
  {:else if activeTab === 'direct'}
    <DirectView />
  {:else if activeTab === 'dashboard'}
    <DashboardView />
  {:else if activeTab === 'history'}
    <HistoryView />
  {:else if activeTab === 'settings'}
    <SettingsView />
  {/if}
</main>