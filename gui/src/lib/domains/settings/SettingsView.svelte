<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import AccountAllowlist from './components/AccountAllowlist.svelte';
  import UrlAllowlist from './components/UrlAllowlist.svelte';
  import DltToggle from './components/DltToggle.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { settingsState } from './settingsState.svelte.js';
  import { onMount } from 'svelte';

  onMount(() => {
    if (!settingsState.settingsLoadedOnce) {
      settingsState.loadSettingsAllowlist();
    }
    if (!settingsState.settingsUrlLoadedOnce) {
      settingsState.loadSettingsUrlAllowlist();
    }
    if (!settingsState.dltLoadedOnce) {
      settingsState.loadDltStatus();
    }
  });

  function refreshAll() {
    settingsState.loadSettingsAllowlist();
    settingsState.loadSettingsUrlAllowlist();
    settingsState.loadDltStatus();
  }

  const isRefreshing = $derived(settingsState.settingsLoading || settingsState.settingsUrlLoading || settingsState.dltLoading);
  const isBusy = $derived(isRefreshing || settingsState.settingsMutating || settingsState.settingsUrlMutating || settingsState.dltMutating);
</script>

<section class="chat-panel">
  <TopBar title="Settings">
    <button onclick={refreshAll} disabled={!daemonState.online || isBusy}>
      {isRefreshing ? 'Refreshing…' : '↻ Refresh'}
    </button>
  </TopBar>

  <div class="dashboard-panel">
    {#if !daemonState.online}
      <p>Daemon offline. Start the daemon to manage settings.</p>
    {:else}
      <p class="settings-mechanism-note">
        Two separate allowlists govern two separate payment paths — they are not
        interchangeable and adding an entry to one has no effect on the other.
      </p>
      <AccountAllowlist />
      <UrlAllowlist />
      <DltToggle />
    {/if}
  </div>
</section>
