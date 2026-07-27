import { tauriInvoke, tauriListen } from '$lib/services/tauri.js';
import { daemonState } from '$lib/services/daemonState.svelte.js';
import { historyState } from '$lib/domains/history/historyState.svelte.js';

/**
 * @typedef {import('$lib/types/chat.js').DaemonLogEvent} DaemonLogEvent
 */

class DirectState {
  directType = $state('fs');
  directTask = $state('');
  /** @type {DaemonLogEvent[]} */
  directEvents = $state([]);
  isDirectSending = $state(false);

  /** @type {Array<() => void>} */
  #unlisten = [];

  async init() {
    const offDirect = await tauriListen('direct-daemon-event', (e) => this.handleDirectEvent(e.payload));
    this.#unlisten.push(offDirect);
  }

  destroy() {
    this.#unlisten.forEach((fn) => fn());
    this.#unlisten = [];
  }

  /** @param {any} event */
  handleDirectEvent(event) {
    const { kind, ...data } = event;

    switch (kind) {
      case 'started':
        this.directEvents = [{
          event_type: 'started',
          payload: {
            content: `${data.skill_type}: ${data.task}`
          }
        }];
        break;

      case 'event':
        this.directEvents = [...this.directEvents, {
          event_type: data.event_type,
          payload: data.payload
        }];
        if (data.event_type === 'payment_settled') {
          historyState.applyPaymentSettled(data.payload ?? {});
        }
        break;

      case 'done':
        this.directEvents = [...this.directEvents, {
          event_type: 'done',
          payload: { content: 'Done' }
        }];
        this.isDirectSending = false;
        break;

      case 'error':
        this.directEvents = [...this.directEvents, {
          event_type: 'error',
          payload: { content: data.message }
        }];
        this.isDirectSending = false;
        break;
    }
  }

  async sendDirectTask() {
    const task = this.directTask.trim();
    const skill_type = this.directType.trim();
    if (!task || !skill_type || this.isDirectSending || !daemonState.online) return;

    this.isDirectSending = true;
    this.directEvents = [];

    try {
      await tauriInvoke('send_direct_task', { task, skillType: skill_type });
    } catch (err) {
      this.directEvents = [...this.directEvents, {
        event_type: 'error',
        payload: { content: `Failed to send task: ${err}` }
      }];
      this.isDirectSending = false;
    }
  }
}

export const directState = new DirectState();
