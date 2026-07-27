<script>
  import { historyState } from '../historyState.svelte.js';
</script>

<section class="history-column history-column-payments">
  <h2>Payment trail</h2>
  {#if historyState.historyPaymentsError}
    <p class="dash-error">⚠ {historyState.historyPaymentsError}</p>
  {:else if !historyState.historyPaymentsLoadedOnce || historyState.historyPaymentsLoading}
    <p>Loading…</p>
  {:else if historyState.historyPayments && historyState.historyPayments.length === 0}
    <p>No payments yet.</p>
  {:else if historyState.historyPayments}
    <table class="dash-table history-payments-table">
      <thead>
        <tr>
          <th>Type</th>
          <th>Transaction</th>
          <th>Recipient</th>
          <th>Amount</th>
          <th>Verified</th>
        </tr>
      </thead>
      <tbody>
        {#each historyState.historyPayments as p (p.transaction_id)}
          <tr>
            <td>
              <span class="history-payment-type">
                {p.skill_called ?? '—'}
              </span>
            </td>
            <td class="history-txid">
              {#if p.hashscan_url}
                <a href={p.hashscan_url} target="_blank" rel="noreferrer">{p.transaction_id}</a>
              {:else}
                {p.transaction_id}
              {/if}
            </td>
            <td>{p.recipient}</td>
            <td>{Number(p.amount_hbar ?? 0).toFixed(4)} ℏ</td>
            <td>
              {#if p.chain_verified === false}
                <span class="history-badge history-badge-unverified">⚠ unverified</span>
              {:else}
                <span class="history-badge history-badge-verified">✓ verified</span>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>
