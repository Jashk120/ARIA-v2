<script>
  import { dashboardState } from '../dashboardState.svelte.js';

  /**
   * Daemon-shaped numbers may be missing; never let `.toFixed` throw in the template.
   * @param {number | null | undefined} v
   */
  function fmtHbar(v) {
    return Number(v ?? 0).toFixed(4);
  }
</script>

<section class="dash-section">
  <h2>Wallet</h2>
  {#if dashboardState.dashboardErrors.wallet}
    <p class="dash-error">⚠ {dashboardState.dashboardErrors.wallet}</p>
  {:else if dashboardState.dashboardWallet}
    <p class="dash-wallet">
      <span class="dash-wallet-account">{dashboardState.dashboardWallet.account_id}</span>
      <span class="dash-wallet-balance">{fmtHbar(dashboardState.dashboardWallet.balance_hbar)} ℏ</span>
    </p>
  {/if}
</section>
