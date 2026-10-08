<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import AccountAllowlist from './components/AccountAllowlist.svelte';
  import UrlAllowlist from './components/UrlAllowlist.svelte';
  import DltToggle from './components/DltToggle.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { settingsState } from './settingsState.svelte.js';
  import { tauriInvoke } from '$lib/services/tauri.js';
  import { onMount } from 'svelte';

  /**
   * @typedef {{ id: string, name: string, url: string, api_key_env: string, default_model: string, models: string[] }} LlmProvider
   * @typedef {{ type: string, provider: string, url_template: string, token: string, url: string, model: string, api_key: string, resolved_url: string, resolved_model: string, providers: LlmProvider[] }} LlmSettings
   * @typedef {{ type: string, settings: LlmSettings }} LlmSaveResult
   */

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
    if (daemonState.online && !llmLoadedOnce && !llmLoading) {
      loadLlmSettings();
    }
  });

  function refreshAll() {
    settingsState.loadSettingsAllowlist();
    settingsState.loadSettingsUrlAllowlist();
    settingsState.loadDltStatus();
    loadLlmSettings();
  }

  // ── LLM / Model settings (daemon-backed) ──
  let provider = $state('');
  /** @type {LlmProvider[]} */
  let providers = $state([]);
  let url = $state('');
  let model = $state('');
  let apiKey = $state('');
  let urlTemplate = $state('');
  let token = $state('');
  let resolvedUrl = $state('');
  let resolvedModel = $state('');

  let llmLoading = $state(false);
  let llmLoadedOnce = $state(false);
  let llmLoadError = $state('');
  let llmSaving = $state(false);
  let llmSaveError = $state('');
  let llmSaved = $state(false);

  const selectedProvider = $derived(providers.find((p) => p.id === provider) ?? null);
  const selectedModels = $derived(selectedProvider?.models ?? []);
  const apiKeyEnvHint = $derived(selectedProvider?.api_key_env ?? '');
  const tokenWarning = $derived(urlTemplate.includes('{TOKEN}') && token === '');

  /** @param {LlmSettings} s */
  function applyLlmSettings(s) {
    providers = Array.isArray(s.providers) ? s.providers : [];
    provider = s.provider ?? '';
    url = s.url ?? '';
    model = s.model ?? '';
    apiKey = s.api_key ?? '';
    urlTemplate = s.url_template ?? '';
    token = s.token ?? '';
    resolvedUrl = s.resolved_url ?? '';
    resolvedModel = s.resolved_model ?? '';
  }

  /** @param {unknown} err */
  function llmErrText(err) {
    return typeof err === 'string' ? err : String(err);
  }

  async function loadLlmSettings() {
    if (!daemonState.online || llmLoading) return;
    llmLoading = true;
    llmLoadError = '';
    try {
      const s = await tauriInvoke('get_llm_settings');
      applyLlmSettings(/** @type {LlmSettings} */ (s));
      llmLoadedOnce = true;
    } catch (e) {
      llmLoadError = llmErrText(e);
    } finally {
      llmLoading = false;
    }
  }

  async function saveLlmSettings() {
    if (!daemonState.online || llmSaving || llmLoading) return;
    llmSaving = true;
    llmSaveError = '';
    llmSaved = false;
    try {
      const res = await tauriInvoke('set_llm_settings', {
        settings: {
          provider,
          url_template: urlTemplate,
          token,
          url,
          model,
          api_key: apiKey
        }
      });
      applyLlmSettings(/** @type {LlmSaveResult} */ (res).settings);
      llmSaved = true;
    } catch (e) {
      llmSaveError = llmErrText(e);
    } finally {
      llmSaving = false;
    }
  }

  function onProviderChange() {
    const p = providers.find((item) => item.id === provider) ?? null;
    if (p) {
      url = p.url ?? '';
      model = p.default_model ?? '';
      apiKey = '';
    }
    llmSaved = false;
    llmSaveError = '';
  }

  function clearLlmSaved() {
    llmSaved = false;
    llmSaveError = '';
  }

  $effect(() => {
    if (daemonState.online && !llmLoadedOnce && !llmLoading) {
      loadLlmSettings();
    }
    if (!daemonState.online) {
      llmLoadedOnce = false;
    }
  });

  const isRefreshing = $derived(settingsState.settingsLoading || settingsState.settingsUrlLoading || settingsState.dltLoading || llmLoading);
  const isBusy = $derived(isRefreshing || settingsState.settingsMutating || settingsState.settingsUrlMutating || settingsState.dltMutating || llmSaving);
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

      <div class="dash-section">
        <h2>LLM / Model</h2>
        <p class="dash-caps-note note-lead">
          Stored values override environment variables (ARIA_LLM_URL, OPENAI_BASE_URL,
          LITELLM_BASE_URL, ARIA_LLM_MODEL, OPENAI_MODEL), which override compiled defaults.
          Leave a field empty to clear it and fall back. Changes apply on the next task turn —
          no daemon restart required.
        </p>

        {#if llmLoading && !llmLoadedOnce}
          <p>Loading LLM settings…</p>
        {:else}
          {#if llmLoadError}
            <p class="dash-error">Failed to load settings: {llmLoadError}</p>
          {/if}

          <form
            onsubmit={(e) => {
              e.preventDefault();
              saveLlmSettings();
            }}
          >
            <label class="field">
              Provider
              <select
                bind:value={provider}
                onchange={onProviderChange}
                disabled={llmLoading || llmSaving}
                aria-label="Provider"
              >
                <option value="">Custom</option>
                {#each providers as p (p.id)}
                  <option value={p.id}>{p.name}</option>
                {/each}
              </select>
            </label>
            {#if apiKeyEnvHint}
              <p class="dash-caps-note note-hint">
                Leave empty to use ${apiKeyEnvHint} from the daemon environment.
              </p>
            {/if}

            <label class="field">
              Base URL
              <input
                type="text"
                placeholder="http://127.0.0.1:8000"
                bind:value={url}
                oninput={clearLlmSaved}
                disabled={llmLoading || llmSaving}
                aria-label="Base URL"
              />
            </label>

            <label class="field">
              Model
              <input
                type="text"
                list="llm-model-list"
                placeholder={selectedModels.length > 0 ? selectedModels[0] : 'gemma-4-31b-it'}
                bind:value={model}
                oninput={clearLlmSaved}
                disabled={llmLoading || llmSaving}
                aria-label="Model"
                autocomplete="off"
              />
              <datalist id="llm-model-list">
                {#each selectedModels as m (m)}
                  <option value={m}></option>
                {/each}
              </datalist>
            </label>
            {#if selectedProvider && selectedModels.length === 0}
              <p class="dash-caps-note note-hint">
                This provider publishes no model list — enter any model name.
              </p>
            {/if}

            <label class="field">
              API key
              <input
                type="password"
                placeholder="sent as Authorization: Bearer <key>"
                bind:value={apiKey}
                oninput={clearLlmSaved}
                disabled={llmLoading || llmSaving}
                aria-label="API key"
              />
            </label>

            <label class="field">
              Custom request URL
              <input
                type="text"
                placeholder={'https://something.com/{TOKEN}/chat'}
                bind:value={urlTemplate}
                oninput={clearLlmSaved}
                disabled={llmLoading || llmSaving}
                aria-label="Custom request URL"
              />
            </label>
            <p class="dash-caps-note note-hint">
              Full endpoint URL used verbatim when set — takes priority over Base URL.
              May contain the literal {'{TOKEN}'}.
            </p>

            <label class="field">
              URL token
              <input
                type="password"
                placeholder={'substituted for {TOKEN}'}
                bind:value={token}
                oninput={clearLlmSaved}
                disabled={llmLoading || llmSaving}
                aria-label="URL token"
              />
            </label>

            {#if tokenWarning}
              <p class="dash-error">
                Warning: custom request URL contains {'{TOKEN}'} but the URL token is empty —
                the daemon substitutes an empty string, producing a broken URL.
              </p>
            {/if}

            {#if llmSaveError}
              <p class="dash-error">Save failed: {llmSaveError}</p>
            {/if}
            {#if llmSaved}
              <p class="settings-notice">Settings saved.</p>
            {/if}

            <div class="confirmation-actions actions">
              <button type="submit" disabled={llmLoading || llmSaving}>
                {llmSaving ? 'Saving…' : 'Save'}
              </button>
            </div>
          </form>
        {/if}
      </div>

      <div class="dash-section">
        <h2>Effective configuration</h2>
        {#if llmLoading && !llmLoadedOnce}
          <p class="dash-caps-note">Loading…</p>
        {:else if llmLoadError && !llmLoadedOnce}
          <p class="dash-error">Unavailable — settings could not be loaded.</p>
        {:else}
          <table class="dash-table">
            <tbody>
              <tr>
                <th>Endpoint</th>
                <td>{resolvedUrl || '(unset)'}</td>
              </tr>
              <tr>
                <th>Model</th>
                <td>{resolvedModel || '(unset)'}</td>
              </tr>
            </tbody>
          </table>
          <p class="dash-caps-note">What the daemon will actually use.</p>
        {/if}
      </div>
    {/if}
  </div>
</section>

<style>
  .field {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    margin-bottom: 1.1rem;
    font-size: 0.82rem;
    color: var(--text-dim);
    font-weight: 600;
  }

  .field input[type='password'],
  .field input[type='text'],
  .field select {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    color: var(--text);
    padding: 0.5rem 0.7rem;
  }

  .field input[type='password']:focus,
  .field input[type='text']:focus,
  .field select:focus {
    outline: none;
    border-color: var(--accent-border);
  }

  .note-lead {
    margin-bottom: 0.85rem;
  }

  .note-hint {
    margin: -0.6rem 0 1.1rem;
  }

  .actions {
    margin-bottom: 0;
  }
</style>
