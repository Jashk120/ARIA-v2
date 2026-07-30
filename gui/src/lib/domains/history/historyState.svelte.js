import { tauriInvoke } from '$lib/services/tauri.js';

/**
 * @typedef {import('$lib/types/history.js').PaymentRecord} PaymentRecord
 * @typedef {import('$lib/types/history.js').HcsRecordItem} HcsRecordItem
 */

class HistoryState {
  /** @type {PaymentRecord[] | null} */
  historyPayments = $state(null);
  historyPaymentsLoading = $state(false);
  historyPaymentsLoadedOnce = $state(false);
  /** @type {string | null} */
  historyPaymentsError = $state(null);

  /** @type {HcsRecordItem[] | null} */
  historyHcs = $state(null);
  historyHcsLoading = $state(false);
  historyHcsLoadedOnce = $state(false);
  /** @type {string | null} */
  historyHcsError = $state(null);

  async loadHistoryPayments() {
    if (this.historyPaymentsLoading) return;
    this.historyPaymentsLoading = true;
    this.historyPaymentsLoadedOnce = true;
    this.historyPaymentsError = null;

    try {
      const result = /** @type {any} */ (await tauriInvoke('dashboard_query', { query: 'query_payment_history' }));
      this.historyPayments = result?.payments ?? [];
    } catch (e) {
      this.historyPaymentsError = String(e);
    } finally {
      this.historyPaymentsLoading = false;
    }
  }

  async loadHistoryHcs() {
    if (this.historyHcsLoading) return;
    this.historyHcsLoading = true;
    this.historyHcsLoadedOnce = true;
    this.historyHcsError = null;

    try {
      const topicResult = /** @type {any} */ (await tauriInvoke('dashboard_query', { query: 'query_audit_topic_id' }));
      const topicId = topicResult?.audit_topic_id ?? topicResult?.topic_id;
      if (!topicId) throw new Error('Daemon did not return an audit_topic_id');

      const res = await fetch(
        `https://testnet.mirrornode.hedera.com/api/v1/topics/${encodeURIComponent(topicId)}/messages?order=desc`
      );
      if (!res.ok) throw new Error(`Mirror Node returned ${res.status}`);
      const body = await res.json();

      this.historyHcs = (body?.messages ?? []).map((/** @type {any} */ m) => {
        try {
          const decoded = atob(m.message);
          return { consensus_timestamp: m.consensus_timestamp, sequence_number: m.sequence_number, record: JSON.parse(decoded) };
        } catch {
          return { consensus_timestamp: m.consensus_timestamp, sequence_number: m.sequence_number, record: null, decodeError: true };
        }
      });
    } catch (e) {
      this.historyHcsError = String(e);
    } finally {
      this.historyHcsLoading = false;
    }
  }

  async loadHistory() {
    this.loadHistoryPayments();
    this.loadHistoryHcs();
  }

  /** @param {Record<string, any>} payload */
  applyPaymentSettled(payload) {
    if (!this.historyPayments || !payload?.transaction_id) return;
    const payment = /** @type {PaymentRecord} */ (payload);
    const idx = this.historyPayments.findIndex((p) => p.transaction_id === payment.transaction_id);
    if (idx === -1) {
      this.historyPayments = [payment, ...this.historyPayments];
    } else {
      this.historyPayments = this.historyPayments.map((p, i) => (i === idx ? { ...p, ...payment } : p));
    }
  }
}

export const historyState = new HistoryState();
