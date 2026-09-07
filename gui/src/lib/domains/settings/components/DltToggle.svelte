<script>
  import { settingsState } from '../settingsState.svelte.js';
</script>

<section class="dash-section settings-section-dlt">
  <h2>DLT skills <span class="dash-section-sub">— air-gap mode</span></h2>
  <p class="dash-caps-note">
    Master switch for all DLT skills (hedera_pay, x402_pay) plus HCS audit
    egress. When off, DLT skills refuse to run and disappear from the model.
    Every flip goes straight to the daemon and takes effect immediately —
    no restart needed.
  </p>

  {#if settingsState.dltLoadError}
    <p class="dash-error">⚠ {settingsState.dltLoadError}</p>
  {:else if !settingsState.dltLoadedOnce || settingsState.dltLoading}
    <p>Loading…</p>
  {:else}
    <label class="settings-dlt-toggle">
      <input
        type="checkbox"
        checked={settingsState.dltEnabled ?? true}
        disabled={settingsState.dltMutating}
        onchange={(e) => settingsState.setDltEnabled(e.currentTarget.checked)}
        aria-label="Enable DLT skills"
      />
      <span>{(settingsState.dltEnabled ?? true) ? 'Enabled' : 'Disabled (air-gap mode)'}</span>
    </label>
    {#if settingsState.dltMutating}
      <p>Working…</p>
    {/if}
  {/if}

  {#if settingsState.dltMutateError}
    <p class="dash-error">⚠ {settingsState.dltMutateError}</p>
  {/if}
  {#if settingsState.dltNotice}
    <p class="settings-notice">{settingsState.dltNotice}</p>
  {/if}
</section>
