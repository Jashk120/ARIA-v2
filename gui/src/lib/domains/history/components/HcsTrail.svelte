<script>
  import { historyState } from '../historyState.svelte.js';
</script>

<section class="history-column history-column-hcs">
  <h2>HCS decision trail</h2>
  <p class="dash-caps-note">
    x402 payments are not logged to HCS by design — per-message Mirror Node cost can
    exceed a micropayment's value, so only hedera_pay decisions ever appear here. A
    missing x402 row on the left isn't a bug.
  </p>
  {#if historyState.historyHcsError}
    <p class="dash-error">⚠ {historyState.historyHcsError}</p>
  {:else if !historyState.historyHcsLoadedOnce || historyState.historyHcsLoading}
    <p>Loading…</p>
  {:else if historyState.historyHcs && historyState.historyHcs.length === 0}
    <p>No HCS decision records found on the audit topic.</p>
  {:else if historyState.historyHcs}
    <ul class="history-hcs-list">
      {#each historyState.historyHcs as msg (msg.sequence_number)}
        <li class="history-hcs-item">
          {#if msg.decodeError || !msg.record}
            <span class="dash-error">⚠ Could not decode message #{msg.sequence_number}</span>
          {:else}
            <div class="history-hcs-header">
              <span class="history-hcs-seq">#{msg.sequence_number}</span>
              <span class="history-hcs-ts">{msg.consensus_timestamp}</span>
            </div>
            <pre class="history-hcs-record">{JSON.stringify(msg.record, null, 2)}</pre>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>
