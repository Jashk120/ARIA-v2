<script>
  import { dashboardState } from '../dashboardState.svelte.js';
</script>

<section class="dash-section">
  <h2>Wallet</h2>
  {#if dashboardState.dashboardErrors.wallet}
    <p class="dash-error">⚠ {dashboardState.dashboardErrors.wallet}</p>
  {:else if dashboardState.dashboardWallet}
    <p class="dash-wallet">
      <span class="dash-wallet-account">{dashboardState.dashboardWallet.account_id}</span>
      <span class="dash-wallet-balance">{dashboardState.dashboardWallet.balance_hbar.toFixed(4)} ℏ</span>
    </p>
  {/if}
</section>
