<script>
  import { historyState } from '../historyState.svelte.js';

  /**
   * @typedef {import('$lib/types/history.js').PaymentRecord} PaymentRecord
   */

  /** @type {PaymentRecord | null} */
  let selectedPayment = $state(null);

  /**
   * @param {PaymentRecord} p
   */
  function selectPayment(p) {
    selectedPayment = selectedPayment?.transaction_id === p.transaction_id ? null : p;
  }

  /**
   * @param {string | undefined} ts
   */
  function formatTimestamp(ts) {
    if (!ts) return '—';
    const d = new Date(ts.includes('T') ? ts : ts.replace(' ', 'T') + 'Z');
    if (isNaN(d.getTime())) return ts;
    return d.toLocaleString(undefined, {
      month: 'short', day: 'numeric', year: 'numeric',
      hour: '2-digit', minute: '2-digit'
    });
  }

  /**
   * @param {string | undefined} status
   */
  function statusClass(status) {
    if (!status) return 'trail-status-unknown';
    const s = status.toUpperCase();
    if (s === 'SUCCESS') return 'trail-status-success';
    if (s === 'FAILED' || s === 'ERROR') return 'trail-status-failed';
    if (s === 'PENDING') return 'trail-status-pending';
    return 'trail-status-unknown';
  }

  /**
   * @param {string} txId
   */
  function copyToClipboard(txId) {
    navigator.clipboard.writeText(txId).catch(() => {});
  }
</script>

{#if historyState.historyPaymentsError}
  <div class="trail-error">
    <span class="trail-error-icon">⚠</span>
    <span>{historyState.historyPaymentsError}</span>
  </div>
{:else if !historyState.historyPaymentsLoadedOnce || historyState.historyPaymentsLoading}
  <div class="trail-loading">
    <span class="trail-spinner"></span>
    Loading payment history…
  </div>
{:else if !historyState.historyPayments || historyState.historyPayments.length === 0}
  <div class="trail-empty">
    <span class="trail-empty-icon">💳</span>
    <p>No payments on record yet.</p>
  </div>
{:else}
  <div class="trail-layout">
    <!-- Trail list -->
    <div class="trail-list">
      <div class="trail-list-header">
        <span>{historyState.historyPayments.length} transaction{historyState.historyPayments.length !== 1 ? 's' : ''}</span>
      </div>
      <ul class="trail-items">
        {#each historyState.historyPayments as p (p.transaction_id)}
          {@const isSelected = selectedPayment?.transaction_id === p.transaction_id}
          <li
            class="trail-item {isSelected ? 'trail-item-selected' : ''}"
            role="button"
            tabindex="0"
            onclick={() => selectPayment(p)}
            onkeydown={(e) => e.key === 'Enter' && selectPayment(p)}
          >
            <div class="trail-item-accent {statusClass(p.status)}"></div>
            <div class="trail-item-body">
              <div class="trail-item-top">
                <span class="trail-item-type">{p.skill_called ?? 'payment'}</span>
                <span class="trail-status-badge {statusClass(p.status)}">
                  {p.status ?? 'unknown'}
                </span>
              </div>
              <div class="trail-item-txid">{p.transaction_id}</div>
              <div class="trail-item-meta">
                <span class="trail-item-recipient" title={p.recipient}>{p.recipient}</span>
                <span class="trail-item-amount">{Number(p.amount_hbar ?? 0).toFixed(4)} ℏ</span>
                <span class="trail-item-ts">{formatTimestamp(p.timestamp)}</span>
              </div>
            </div>
            <div class="trail-item-arrow">{isSelected ? '▾' : '▸'}</div>
          </li>
        {/each}
      </ul>
    </div>

    <!-- Detail panel -->
    {#if selectedPayment}
      <div class="trail-detail">
        <div class="trail-detail-header">
          <span class="trail-detail-title">Transaction Detail</span>
          <button class="trail-detail-close" onclick={() => (selectedPayment = null)}>✕</button>
        </div>

        <div class="trail-detail-section">
          <div class="trail-detail-label">Type</div>
          <div class="trail-detail-value">
            <span class="trail-item-type">{selectedPayment.skill_called ?? '—'}</span>
          </div>
        </div>

        <div class="trail-detail-section">
          <div class="trail-detail-label">Status</div>
          <div class="trail-detail-value">
            <span class="trail-status-badge {statusClass(selectedPayment.status)}">
              {selectedPayment.status ?? 'unknown'}
            </span>
            &nbsp;
            {#if selectedPayment.chain_verified}
              <span class="history-badge history-badge-verified">✓ chain verified</span>
            {:else}
              <span class="history-badge history-badge-unverified">⚠ not chain verified</span>
            {/if}
          </div>
        </div>

        <div class="trail-detail-section">
          <div class="trail-detail-label">Transaction ID</div>
          <div class="trail-detail-value trail-detail-mono">
            {selectedPayment.transaction_id}
            <button
              class="trail-copy-btn"
              title="Copy transaction ID"
              onclick={() => copyToClipboard(selectedPayment?.transaction_id ?? '')}
            >📋</button>
          </div>
        </div>

        {#if selectedPayment.hashscan_url}
          <div class="trail-detail-section">
            <div class="trail-detail-label">Hashscan</div>
            <div class="trail-detail-value">
              <a href={selectedPayment.hashscan_url} target="_blank" rel="noreferrer" class="trail-detail-link">
                View on Hashscan ↗
              </a>
            </div>
          </div>
        {/if}

        <div class="trail-detail-section">
          <div class="trail-detail-label">Recipient</div>
          <div class="trail-detail-value trail-detail-mono">
            {selectedPayment.recipient}
            <button
              class="trail-copy-btn"
              title="Copy recipient"
              onclick={() => copyToClipboard(selectedPayment?.recipient ?? '')}
            >📋</button>
          </div>
        </div>

        <div class="trail-detail-section">
          <div class="trail-detail-label">Amount</div>
          <div class="trail-detail-value trail-detail-amount">
            {Number(selectedPayment.amount_hbar ?? 0).toFixed(8)} ℏ
          </div>
        </div>

        {#if selectedPayment.timestamp}
          <div class="trail-detail-section">
            <div class="trail-detail-label">Timestamp</div>
            <div class="trail-detail-value">{formatTimestamp(selectedPayment.timestamp)}</div>
          </div>
        {/if}
      </div>
    {:else}
      <div class="trail-detail trail-detail-empty">
        <span class="trail-detail-empty-icon">👆</span>
        <p>Select a transaction to view details</p>
      </div>
    {/if}
  </div>
{/if}
