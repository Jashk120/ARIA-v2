<script>
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { dashboardState } from '../dashboardState.svelte.js';
</script>

<section class="dash-section">
  <h2>Holds — pending / uncommitted</h2>
  {#if dashboardState.holdActionError}
    <p class="dash-error">⚠ {dashboardState.holdActionError}</p>
  {/if}
  {#if dashboardState.dashboardErrors.holds}
    <p class="dash-error">⚠ {dashboardState.dashboardErrors.holds}</p>
  {:else if dashboardState.dashboardHolds && dashboardState.dashboardHolds.length === 0}
    <p>No holds.</p>
  {:else if dashboardState.dashboardHolds}
    <table class="dash-table">
      <thead>
        <tr><th>Payment key</th><th>Amount</th><th>Timestamp</th><th>Action</th></tr>
      </thead>
      <tbody>
        {#each dashboardState.dashboardHolds as hold (hold.payment_key)}
          <tr>
            <td>{hold.payment_key}</td>
            <td>{hold.amount_hbar.toFixed(4)} ℏ</td>
            <td>{hold.timestamp}</td>
            <td>
              {#if dashboardState.holdPendingAction && dashboardState.holdPendingAction.payment_key === hold.payment_key}
                <span class="hold-confirm">
                  Confirm {dashboardState.holdPendingAction.action === 'approve' ? 'approval' : 'release'} of {hold.amount_hbar.toFixed(4)} ℏ?
                  <button onclick={() => dashboardState.confirmHoldAction()} disabled={dashboardState.holdActionInFlight}>
                    {dashboardState.holdActionInFlight ? 'Working…' : 'Confirm'}
                  </button>
                  <button onclick={() => dashboardState.cancelHoldAction()} disabled={dashboardState.holdActionInFlight}>Cancel</button>
                </span>
              {:else}
                <div class="hold-actions">
                  <button
                    onclick={() => dashboardState.requestHoldAction(hold.payment_key, 'approve')}
                    disabled={!daemonState.online || dashboardState.holdActionInFlight || !!dashboardState.holdPendingAction}
                  >Approve</button>
                  <button
                    onclick={() => dashboardState.requestHoldAction(hold.payment_key, 'release')}
                    disabled={!daemonState.online || dashboardState.holdActionInFlight || !!dashboardState.holdPendingAction}
                  >Release</button>
                </div>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>
