import { tauriInvoke } from './tauri.js';

class DaemonState {
  online = $state(false);
  /** @type {ReturnType<typeof setInterval> | undefined} */
  #pollInterval;

  async check() {
    this.online = await tauriInvoke('check_daemon').catch(() => false);
  }

  init() {
    this.check();
    this.#pollInterval = setInterval(() => this.check(), 3000);
  }

  destroy() {
    if (this.#pollInterval) {
      clearInterval(this.#pollInterval);
    }
  }
}

export const daemonState = new DaemonState();
