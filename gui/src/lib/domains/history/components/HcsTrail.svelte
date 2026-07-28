<script>
  import { historyState } from '../historyState.svelte.js';

  /**
   * @typedef {import('$lib/types/history.js').HcsRecordItem} HcsRecordItem
   */

  /** @type {Set<number>} */
  let expandedSeqs = $state(new Set());

  /**
   * @param {number} seq
   */
  function toggleExpand(seq) {
    const next = new Set(expandedSeqs);
    if (next.has(seq)) {
      next.delete(seq);
    } else {
      next.add(seq);
    }
    expandedSeqs = next;
  }

  /** @type {HcsRecordItem | null} */
  let selectedHcs = $state(null);

  /**
   * @param {HcsRecordItem} msg
   */
  function selectHcs(msg) {
    selectedHcs = selectedHcs?.sequence_number === msg.sequence_number ? null : msg;
  }

  /**
   * @param {string} ts
   */
  function formatHcsTimestamp(ts) {
    if (!ts) return '—';
    // HCS timestamps are like "1690000000.123456789"
    const seconds = parseFloat(ts);
    if (isNaN(seconds)) return ts;
    const d = new Date(seconds * 1000);
    return d.toLocaleString(undefined, {
      month: 'short', day: 'numeric', year: 'numeric',
      hour: '2-digit', minute: '2-digit', second: '2-digit'
    });
  }

  /**
   * Get a short human-readable summary of a decoded record.
   * @param {Record<string, any> | null} record
   */
  function recordSummary(record) {
    if (!record) return 'Unknown record';
    // Try common ARIA audit record fields
    if (record.action) return String(record.action);
    if (record.decision) return String(record.decision);
    if (record.event) return String(record.event);
    if (record.type) return String(record.type);
    if (record.skill_called) return `skill: ${record.skill_called}`;
    // Fall back to first key
    const firstKey = Object.keys(record)[0];
    if (firstKey) return `${firstKey}: ${JSON.stringify(record[firstKey])}`.slice(0, 60);
    return 'Record';
  }

  /**
   * @param {Record<string, any> | null} record
   */
  function recordDecisionClass(record) {
    if (!record) return '';
    const decision = (record.decision ?? record.action ?? record.status ?? '').toString().toLowerCase();
    if (decision.includes('approve') || decision.includes('allow') || decision.includes('success')) return 'hcs-decision-approved';
    if (decision.includes('reject') || decision.includes('deny') || decision.includes('fail')) return 'hcs-decision-rejected';
    if (decision.includes('hold') || decision.includes('pending') || decision.includes('await')) return 'hcs-decision-pending';
    return '';
  }
</script>

{#if historyState.historyHcsError}
  <div class="trail-error">
    <span class="trail-error-icon">⚠</span>
    <span>{historyState.historyHcsError}</span>
  </div>
{:else if !historyState.historyHcsLoadedOnce || historyState.historyHcsLoading}
  <div class="trail-loading">
    <span class="trail-spinner"></span>
    Loading HCS decision trail…
  </div>
{:else if !historyState.historyHcs || historyState.historyHcs.length === 0}
  <div class="trail-empty">
    <span class="trail-empty-icon">🔗</span>
    <p>No HCS decision records found on the audit topic.</p>
    <p class="trail-empty-note">
      x402 payments are not logged to HCS by design — only <code>hedera_pay</code> decisions appear here.
    </p>
  </div>
{:else}
  <div class="trail-layout">
    <!-- HCS trail list -->
    <div class="trail-list">
      <div class="trail-list-header">
        <span>{historyState.historyHcs.length} HCS message{historyState.historyHcs.length !== 1 ? 's' : ''}</span>
        <span class="trail-list-note">x402 payments excluded by design</span>
      </div>
      <ul class="trail-items">
        {#each historyState.historyHcs as msg (msg.sequence_number)}
          {@const isSelected = selectedHcs?.sequence_number === msg.sequence_number}
          <li
            class="trail-item {isSelected ? 'trail-item-selected' : ''} {msg.decodeError ? 'trail-item-error' : ''}"
            role="button"
            tabindex="0"
            onclick={() => !msg.decodeError && selectHcs(msg)}
            onkeydown={(e) => e.key === 'Enter' && !msg.decodeError && selectHcs(msg)}
          >
            <div class="trail-item-accent hcs-accent {recordDecisionClass(msg.record)}"></div>
            <div class="trail-item-body">
              <div class="trail-item-top">
                <span class="hcs-seq-badge">#{msg.sequence_number}</span>
                {#if msg.decodeError}
                  <span class="trail-status-badge trail-status-failed">decode error</span>
                {:else if msg.record}
                  <span class="hcs-summary-label">{recordSummary(msg.record)}</span>
                {/if}
              </div>
              <div class="trail-item-ts">{formatHcsTimestamp(msg.consensus_timestamp)}</div>
            </div>
            {#if !msg.decodeError}
              <div class="trail-item-arrow">{isSelected ? '▾' : '▸'}</div>
            {/if}
          </li>
        {/each}
      </ul>
    </div>

    <!-- Detail panel -->
    {#if selectedHcs}
      <div class="trail-detail">
        <div class="trail-detail-header">
          <span class="trail-detail-title">HCS Message #{selectedHcs.sequence_number}</span>
          <button class="trail-detail-close" onclick={() => (selectedHcs = null)}>✕</button>
        </div>

        <div class="trail-detail-section">
          <div class="trail-detail-label">Consensus Timestamp</div>
          <div class="trail-detail-value">{formatHcsTimestamp(selectedHcs.consensus_timestamp)}</div>
        </div>

        <div class="trail-detail-section">
          <div class="trail-detail-label">Raw Timestamp</div>
          <div class="trail-detail-value trail-detail-mono">{selectedHcs.consensus_timestamp}</div>
        </div>

        {#if selectedHcs.record}
          {@const rec = selectedHcs.record}
          {#each Object.entries(rec) as [key, value]}
            <div class="trail-detail-section">
              <div class="trail-detail-label">{key}</div>
              <div class="trail-detail-value trail-detail-mono">
                {typeof value === 'object' ? JSON.stringify(value, null, 2) : String(value)}
              </div>
            </div>
          {/each}

          <div class="trail-detail-section">
            <div class="trail-detail-label">Full JSON</div>
            <pre class="trail-detail-json">{JSON.stringify(selectedHcs.record, null, 2)}</pre>
          </div>
        {/if}
      </div>
    {:else}
      <div class="trail-detail trail-detail-empty">
        <span class="trail-detail-empty-icon">🔗</span>
        <p>Select an HCS message to view details</p>
        <p class="trail-empty-note">
          Each message is an on-chain audit record of a payment governance decision.
        </p>
      </div>
    {/if}
  </div>
{/if}
