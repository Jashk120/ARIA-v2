<script>
  import { settingsState } from '../settingsState.svelte.js';
</script>

<section class="dash-section settings-section-url">
  <h2>URL allowlist <span class="dash-section-sub">— governs x402_pay</span></h2>
  <p class="dash-caps-note">
    A wholly separate mechanism from the account allowlist above. Only URLs listed
    here can be autonomously paid via x402_pay's micropayment path. Every add/remove
    goes straight to the daemon and the list below is always re-fetched from it
    afterward, never guessed locally.
  </p>

  <form
    class="settings-add-form"
    onsubmit={(e) => { e.preventDefault(); settingsState.addUrlAllowlistEntry(); }}
  >
    <input
      type="text"
      bind:value={settingsState.settingsNewUrl}
      placeholder="https://api.example.com/resource"
      disabled={settingsState.settingsUrlMutating}
      aria-label="URL to add"
    />
    <button type="submit" disabled={settingsState.settingsUrlMutating || !settingsState.settingsNewUrl.trim()}>
      {settingsState.settingsUrlMutating ? 'Working…' : 'Add'}
    </button>
  </form>

  {#if settingsState.settingsUrlMutateError}
    <p class="dash-error">⚠ {settingsState.settingsUrlMutateError}</p>
  {/if}
  {#if settingsState.settingsUrlMutateNotice}
    <p class="settings-notice">{settingsState.settingsUrlMutateNotice}</p>
  {/if}

  {#if settingsState.settingsUrlLoadError}
    <p class="dash-error">⚠ {settingsState.settingsUrlLoadError}</p>
  {:else if !settingsState.settingsUrlLoadedOnce || settingsState.settingsUrlLoading}
    <p>Loading…</p>
  {:else if settingsState.settingsUrlAllowlist && settingsState.settingsUrlAllowlist.length === 0}
    <p>No allowlisted URLs.</p>
  {:else if settingsState.settingsUrlAllowlist}
    <ul class="settings-allowlist">
      {#each settingsState.settingsUrlAllowlist as url (url)}
        <li>
          <span class="settings-account settings-url">
            {url}
            {#if settingsState.settingsUrlRateStatus[url] === 'error'}
              <span class="settings-rate-status settings-rate-status-error">rate status unavailable</span>
            {:else if settingsState.settingsUrlRateStatus[url] && typeof settingsState.settingsUrlRateStatus[url] === 'object'}
              <span class="settings-rate-status">
                {settingsState.settingsUrlRateStatus[url].count}/{settingsState.settingsUrlRateStatus[url].limit} {settingsState.settingsUrlRateStatus[url].window ?? 'this hour'}
              </span>
            {/if}
          </span>
          <button
            class="settings-remove-btn"
            onclick={() => settingsState.removeUrlAllowlistEntry(url)}
            disabled={settingsState.settingsUrlMutating}
            aria-label={`Remove ${url}`}
          >Remove</button>
        </li>
      {/each}
    </ul>
  {/if}
</section>
