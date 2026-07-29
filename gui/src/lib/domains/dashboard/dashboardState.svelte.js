import { tauriInvoke } from '$lib/services/tauri.js';
import { daemonState } from '$lib/services/daemonState.svelte.js';
import { historyState } from '$lib/domains/history/historyState.svelte.js';
import { chatState } from '$lib/domains/chat/chatState.svelte.js';

/**
 * @typedef {import('$lib/types/dashboard.js').BudgetInfo} BudgetInfo
 * @typedef {import('$lib/types/dashboard.js').HoldRecord} HoldRecord
 * @typedef {import('$lib/types/dashboard.js').WalletInfo} WalletInfo
 */

class DashboardState {
  /** @type {BudgetInfo | null} */
  dashboardBudget = $state(null);
  /** @type {HoldRecord[] | null} */
  dashboardHolds = $state(null);
  /** @type {string[] | null} */
  dashboardAllowlist = $state(null);
  /** @type {WalletInfo | null} */
  dashboardWallet = $state(null);
  dashboardLoading = $state(false);
  /** @type {Record<string, string>} */
  dashboardErrors = $state({});
  dashboardLoadedOnce = $state(false);

  // ── Holds Approve/Release actions ──
  /** @type {{ payment_key: string, action: 'approve' | 'release' } | null} */
  holdPendingAction = $state(null);
  holdActionInFlight = $state(false);
  /** @type {string | null} */
  holdActionError = $state(null);

  async loadDashboard() {
    if (this.dashboardLoading) return;
    this.dashboardLoading = true;
    this.dashboardLoadedOnce = true;
    /** @type {Record<string, string>} */
    const errors = {};

    const [budget, holds, allowlist, wallet] = await Promise.all([
      tauriInvoke('dashboard_query', { query: 'query_budget' }).catch((e) => { errors.budget = String(e); return null; }),
      tauriInvoke('dashboard_query', { query: 'query_holds' }).catch((e) => { errors.holds = String(e); return null; }),
      tauriInvoke('dashboard_query', { query: 'query_allowlist' }).catch((e) => { errors.allowlist = String(e); return null; }),
      tauriInvoke('dashboard_query', { query: 'query_wallet_balance' }).catch((e) => { errors.wallet = String(e); return null; }),
    ]);

    this.dashboardBudget = /** @type {any} */ (budget);
    this.dashboardHolds = /** @type {any} */ (holds)?.holds ?? null;
    this.dashboardAllowlist = /** @type {any} */ (allowlist)?.accounts ?? null;
    this.dashboardWallet = /** @type {any} */ (wallet);
    this.dashboardErrors = errors;
    this.dashboardLoading = false;
  }

  /**
   * @param {string} paymentKey
   * @param {'approve' | 'release'} action
   */
  requestHoldAction(paymentKey, action) {
    if (this.holdActionInFlight) return;
    this.holdActionError = null;
    this.holdPendingAction = { payment_key: paymentKey, action };
  }

  cancelHoldAction() {
    if (this.holdActionInFlight) return;
    this.holdPendingAction = null;
    this.holdActionError = null;
  }

  async confirmHoldAction() {
    if (!this.holdPendingAction || this.holdActionInFlight || !daemonState.online) return;
    const { payment_key, action } = this.holdPendingAction;
    this.holdActionInFlight = true;
    this.holdActionError = null;

    try {
      await tauriInvoke(action === 'approve' ? 'approve_hold' : 'release_hold', { paymentKey: payment_key });
      chatState.handleDashboardHoldAction(action);
      this.holdPendingAction = null;
      await this.loadDashboard();
      if (historyState.historyPaymentsLoadedOnce) {
        await historyState.loadHistoryPayments();
      }
    } catch (e) {
      this.holdActionError = String(e);
    } finally {
      this.holdActionInFlight = false;
    }
  }
}

export const dashboardState = new DashboardState();
