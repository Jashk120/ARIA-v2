<script>
  import { dashboardState } from '../dashboardState.svelte.js';
</script>

<section class="dash-section">
  <h2>Account allowlist <span class="dash-section-sub">(hedera_pay)</span></h2>
  {#if dashboardState.dashboardErrors.allowlist}
    <p class="dash-error">⚠ {dashboardState.dashboardErrors.allowlist}</p>
  {:else if dashboardState.dashboardAllowlist && dashboardState.dashboardAllowlist.length === 0}
    <p>No allowlisted accounts.</p>
  {:else if dashboardState.dashboardAllowlist}
    <ul class="dash-list">
      {#each dashboardState.dashboardAllowlist as account}
        <li>{account}</li>
      {/each}
    </ul>
  {/if}
  <p class="dash-caps-note">Display only — manage entries from Settings.</p>
</section>
