import { tauriInvoke } from '$lib/services/tauri.js';
import { daemonState } from '$lib/services/daemonState.svelte.js';
import { dashboardState } from '$lib/domains/dashboard/dashboardState.svelte.js';

/**
 * @typedef {import('$lib/types/settings.js').UrlRateStatusMap} UrlRateStatusMap
 */

class SettingsState {
  // ── Account Allowlist ──
  /** @type {string[] | null} */
  settingsAllowlist = $state(null);
  settingsLoading = $state(false);
  settingsLoadedOnce = $state(false);
  /** @type {string | null} */
  settingsLoadError = $state(null);
  settingsNewAccount = $state('');
  settingsMutating = $state(false);
  /** @type {string | null} */
  settingsMutateError = $state(null);
  /** @type {string | null} */
  settingsMutateNotice = $state(null);

  // ── URL Allowlist ──
  /** @type {string[] | null} */
  settingsUrlAllowlist = $state(null);
  settingsUrlLoading = $state(false);
  settingsUrlLoadedOnce = $state(false);
  /** @type {string | null} */
  settingsUrlLoadError = $state(null);
  settingsNewUrl = $state('');
  settingsUrlMutating = $state(false);
  /** @type {string | null} */
  settingsUrlMutateError = $state(null);
  /** @type {string | null} */
  settingsUrlMutateNotice = $state(null);
  /** @type {UrlRateStatusMap} */
  settingsUrlRateStatus = $state({});

  async loadSettingsAllowlist() {
    if (this.settingsLoading) return;
    this.settingsLoading = true;
    this.settingsLoadedOnce = true;
    this.settingsLoadError = null;

    try {
      const result = /** @type {any} */ (await tauriInvoke('dashboard_query', { query: 'query_allowlist' }));
      this.settingsAllowlist = result?.accounts ?? [];
      if (dashboardState.dashboardLoadedOnce) {
        dashboardState.dashboardAllowlist = this.settingsAllowlist;
      }
    } catch (e) {
      this.settingsLoadError = String(e);
    } finally {
      this.settingsLoading = false;
    }
  }

  async addAllowlistAccount() {
    const account = this.settingsNewAccount.trim();
    if (!account || this.settingsMutating || !daemonState.online) return;

    this.settingsMutating = true;
    this.settingsMutateError = null;
    this.settingsMutateNotice = null;

    try {
      const result = /** @type {any} */ (await tauriInvoke('mutate_allowlist', { action: 'add', account }));
      this.settingsMutateNotice = result?.changed
        ? `Added ${account} to the allowlist.`
        : `${account} was already on the allowlist.`;
      this.settingsNewAccount = '';
      await this.loadSettingsAllowlist();
    } catch (e) {
      this.settingsMutateError = String(e);
    } finally {
      this.settingsMutating = false;
    }
  }

  /** @param {string} account */
  async removeAllowlistAccount(account) {
    if (this.settingsMutating || !daemonState.online) return;

    this.settingsMutating = true;
    this.settingsMutateError = null;
    this.settingsMutateNotice = null;

    try {
      const result = /** @type {any} */ (await tauriInvoke('mutate_allowlist', { action: 'remove', account }));
      this.settingsMutateNotice = result?.changed
        ? `Removed ${account} from the allowlist.`
        : `${account} was not on the allowlist.`;
      await this.loadSettingsAllowlist();
    } catch (e) {
      this.settingsMutateError = String(e);
    } finally {
      this.settingsMutating = false;
    }
  }

  async loadSettingsUrlAllowlist() {
    if (this.settingsUrlLoading) return;
    this.settingsUrlLoading = true;
    this.settingsUrlLoadedOnce = true;
    this.settingsUrlLoadError = null;

    try {
      const result = /** @type {any} */ (await tauriInvoke('dashboard_query', { query: 'query_url_allowlist' }));
      this.settingsUrlAllowlist = result?.urls ?? [];
      this.settingsUrlLoading = false;
      await this.refreshUrlRateStatuses(this.settingsUrlAllowlist ?? []);
    } catch (e) {
      this.settingsUrlLoadError = String(e);
      this.settingsUrlLoading = false;
    }
  }

  /** @param {string[]} urls */
  async refreshUrlRateStatuses(urls) {
    /** @type {UrlRateStatusMap} */
    const next = {};
    await Promise.all(urls.map(async (url) => {
      try {
        const status = /** @type {any} */ (await tauriInvoke('query_url_rate_status', { url }));
        next[url] = { count: status?.count, limit: status?.limit, window: status?.window };
      } catch {
        next[url] = 'error';
      }
    }));
    this.settingsUrlRateStatus = next;
  }

  async addUrlAllowlistEntry() {
    const url = this.settingsNewUrl.trim();
    if (!url || this.settingsUrlMutating || !daemonState.online) return;

    this.settingsUrlMutating = true;
    this.settingsUrlMutateError = null;
    this.settingsUrlMutateNotice = null;

    try {
      const result = /** @type {any} */ (await tauriInvoke('mutate_url_allowlist', { action: 'add', url }));
      this.settingsUrlMutateNotice = result?.changed
        ? `Added ${url} to the URL allowlist.`
        : `${url} was already on the URL allowlist.`;
      this.settingsNewUrl = '';
      await this.loadSettingsUrlAllowlist();
    } catch (e) {
      this.settingsUrlMutateError = String(e);
    } finally {
      this.settingsUrlMutating = false;
    }
  }

  /** @param {string} url */
  async removeUrlAllowlistEntry(url) {
    if (this.settingsUrlMutating || !daemonState.online) return;

    this.settingsUrlMutating = true;
    this.settingsUrlMutateError = null;
    this.settingsUrlMutateNotice = null;

    try {
      const result = /** @type {any} */ (await tauriInvoke('mutate_url_allowlist', { action: 'remove', url }));
      this.settingsUrlMutateNotice = result?.changed
        ? `Removed ${url} from the URL allowlist.`
        : `${url} was not on the URL allowlist.`;
      await this.loadSettingsUrlAllowlist();
    } catch (e) {
      this.settingsUrlMutateError = String(e);
    } finally {
      this.settingsUrlMutating = false;
    }
  }
}

export const settingsState = new SettingsState();
