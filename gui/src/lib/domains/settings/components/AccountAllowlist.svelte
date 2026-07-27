<script>
  import { settingsState } from '../settingsState.svelte.js';
</script>

<section class="dash-section settings-section-account">
  <h2>Account allowlist <span class="dash-section-sub">— governs hedera_pay</span></h2>
  <p class="dash-caps-note">
    Only Hedera accounts listed here can receive payments the agent initiates via
    hedera_pay. Every add/remove goes straight to the daemon and the list below is
    always re-fetched from it afterward, never guessed locally.
  </p>

  <form
    class="settings-add-form"
    onsubmit={(e) => { e.preventDefault(); settingsState.addAllowlistAccount(); }}
  >
    <input
      type="text"
      bind:value={settingsState.settingsNewAccount}
      placeholder="0.0.12345"
      disabled={settingsState.settingsMutating}
      aria-label="Account to add"
    />
    <button type="submit" disabled={settingsState.settingsMutating || !settingsState.settingsNewAccount.trim()}>
      {settingsState.settingsMutating ? 'Working…' : 'Add'}
    </button>
  </form>

  {#if settingsState.settingsMutateError}
    <p class="dash-error">⚠ {settingsState.settingsMutateError}</p>
  {/if}
  {#if settingsState.settingsMutateNotice}
    <p class="settings-notice">{settingsState.settingsMutateNotice}</p>
  {/if}

  {#if settingsState.settingsLoadError}
    <p class="dash-error">⚠ {settingsState.settingsLoadError}</p>
  {:else if !settingsState.settingsLoadedOnce || settingsState.settingsLoading}
    <p>Loading…</p>
  {:else if settingsState.settingsAllowlist && settingsState.settingsAllowlist.length === 0}
    <p>No allowlisted accounts.</p>
  {:else if settingsState.settingsAllowlist}
    <ul class="settings-allowlist">
      {#each settingsState.settingsAllowlist as account (account)}
        <li>
          <span class="settings-account">{account}</span>
          <button
            class="settings-remove-btn"
            onclick={() => settingsState.removeAllowlistAccount(account)}
            disabled={settingsState.settingsMutating}
            aria-label={`Remove ${account}`}
          >Remove</button>
        </li>
      {/each}
    </ul>
  {/if}
</section>
