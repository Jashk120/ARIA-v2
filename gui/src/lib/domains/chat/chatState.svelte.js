import { tauriInvoke, tauriListen } from '$lib/services/tauri.js';
import { daemonState } from '$lib/services/daemonState.svelte.js';
import { historyState } from '$lib/domains/history/historyState.svelte.js';

let msgId = 0;

/**
 * @typedef {import('$lib/types/chat.js').UiMessage} UiMessage
 * @typedef {import('$lib/types/chat.js').Session} Session
 * @typedef {import('$lib/types/chat.js').ChatHistoryItem} ChatHistoryItem
 * @typedef {import('$lib/types/chat.js').DaemonLogEvent} DaemonLogEvent
 */

class ChatState {
  /** @type {UiMessage[]} */
  messages = $state([]);
  /** @type {ChatHistoryItem[]} */
  history = $state([]);
  /** @type {Session[]} */
  sessions = $state([]);
  /** @type {string | null} */
  currentSession = $state(null);
  isThinking = $state(false);
  /** @type {string | null} */
  activeDaemonSkillType = $state(null);

  /** @type {string | null} */
  #currentGroupId = null;
  /** @type {Array<() => void>} */
  #unlisten = [];

  /** @type {HTMLElement | undefined} */
  messagesEl = $state(undefined);

  async init() {
    const offAria = await tauriListen('aria-event', (e) => this.handleAriaEvent(e.payload));
    this.#unlisten.push(offAria);

    await this.refreshSessions();

    if (this.sessions.length > 0) {
      await this.openSession(this.sessions[0].id);
    } else {
      await this.newSession();
    }
  }

  destroy() {
    this.#unlisten.forEach((fn) => fn());
    this.#unlisten = [];
  }

  // ── Persistence helpers ───────────────────────────────────────────────────

  /**
   * @param {string} role
   * @param {string} content
   * @param {string} eventType
   * @param {Record<string, any> | null} [payload]
   * @param {string | null} [groupId]
   */
  persistEvent(role, content, eventType, payload = null, groupId = null) {
    if (!this.currentSession) return;
    tauriInvoke('save_event', {
      sessionId: this.currentSession,
      role,
      content,
      eventType,
      payloadJson: payload ? JSON.stringify(payload) : null,
      groupId
    }).catch(() => {});
  }

  /**
   * @param {string} task
   * @param {string} skillType
   * @param {DaemonLogEvent[]} events
   */
  summarizeDaemonGroup(task, skillType, events) {
    const outcome = [...events].reverse().find((ev) => ev.event_type === 'final' || ev.event_type === 'chat');
    const errorEv = events.find((ev) => ev.event_type === 'error');
    const result = outcome?.payload?.content ?? (errorEv ? `Error: ${errorEv.payload?.content}` : '(no result)');
    return `Delegated task (${skillType}): ${task}\nResult: ${result}`;
  }

  // ── Session Management ─────────────────────────────────────────────────────

  async refreshSessions() {
    const raw = /** @type {[string, string, number][]} */ (await tauriInvoke('list_sessions').catch(() => []));
    this.sessions = raw.map(([id, title, ts]) => ({ id, title, ts }));
  }

  async newSession() {
    const id = `sess_${Date.now()}`;
    await tauriInvoke('create_session', { sessionId: id, title: 'New Chat' });
    await this.refreshSessions();
    await this.openSession(id);
  }

  /** @param {string} id */
  async openSession(id) {
    this.currentSession = id;
    this.history = [];
    this.messages = [];
    this.activeDaemonSkillType = null;

    const stored = /** @type {any[]} */ (await tauriInvoke('load_messages', { sessionId: id }).catch(() => []));
    /** @type {Map<string, UiMessage>} */
    const groupBlocks = new Map();

    for (const row of stored) {
      const payload = row.payload_json ? JSON.parse(row.payload_json) : {};

      if (row.group_id) {
        let block = groupBlocks.get(row.group_id);
        if (row.event_type === 'daemon_task') {
          if (!block) {
            block = {
              id: ++msgId,
              role: 'daemon_block',
              content: row.content,
              skillType: payload.skill_type ?? 'task',
              daemonEvents: [],
              streaming: false,
              groupId: row.group_id
            };
            groupBlocks.set(row.group_id, block);
            this.messages = [...this.messages, block];
          }
        } else if (block) {
          const answeredAsk = row.event_type === 'user_reply' && payload.task_id
            ? [...(block.daemonEvents ?? [])].reverse().find(
                (ev) => ev.event_type === 'ask' && ev.payload?.task_id === payload.task_id && !ev.resolved
              )
            : undefined;

          if (answeredAsk) {
            answeredAsk.resolved = true;
            answeredAsk.reply = payload.content;
            this.messages = this.messages;
          } else {
            block.daemonEvents = [...(block.daemonEvents ?? []), { event_type: row.event_type, payload }];
            this.messages = this.messages;
          }

          if (row.event_type === 'final' || row.event_type === 'chat' || row.event_type === 'error') {
            this.history = [
              ...this.history,
              { role: 'assistant', content: this.summarizeDaemonGroup(block.content, block.skillType ?? 'task', block.daemonEvents ?? []) }
            ];
          }
        }
        continue;
      }

      if (row.event_type === 'text') {
        this.messages = [...this.messages, { id: ++msgId, role: row.role, content: row.content }];
        this.history = [...this.history, { role: row.role, content: row.content }];
      } else if (row.event_type === 'ask_self') {
        this.messages = [...this.messages, { id: ++msgId, role: 'ask_self', content: row.content }];
        this.history = [...this.history, { role: 'assistant', content: row.content }];
      } else if (row.event_type === 'error') {
        this.messages = [...this.messages, { id: ++msgId, role: 'error', content: row.content }];
      }
    }

    const pending = /** @type {{ task_id: string, content: string, kind?: string, skill_type: string } | null} */ (
      await tauriInvoke('load_pending_confirmation', { sessionId: id }).catch(() => null)
    );
    if (pending) {
      this.activeDaemonSkillType = pending.skill_type;
      const block = this.lastDaemonBlock();
      if (block) {
        block.daemonEvents = [...(block.daemonEvents ?? []), {
          event_type: 'ask',
          payload: { content: pending.content, kind: pending.kind },
          taskId: pending.task_id,
          skillType: pending.skill_type,
          groupId: block.groupId,
          resolved: false,
          confirmationReply: ''
        }];
        this.messages = this.messages;
      } else {
        this.messages = [...this.messages, {
          id: ++msgId,
          role: 'awaiting_confirmation',
          content: pending.content,
          taskId: pending.task_id,
          confirmationKind: pending.kind,
          skillType: pending.skill_type,
          confirmationReply: ''
        }];
      }
    }
    this.scrollBottom();
  }

  /** @param {string} id */
  async deleteSession(id) {
    await tauriInvoke('delete_session', { sessionId: id });
    await this.refreshSessions();
    if (id === this.currentSession) {
      if (this.sessions.length > 0) {
        await this.openSession(this.sessions[0].id);
      } else {
        await this.newSession();
      }
    }
  }

  // ── Event Handling ─────────────────────────────────────────────────────────

  /** @param {any} event */
  handleAriaEvent(event) {
    const { kind, ...data } = event;

    switch (kind) {
      case 'token': {
        const last = this.messages[this.messages.length - 1];
        if (last && last.role === 'assistant' && last.streaming) {
          last.content += data.content;
          this.messages = this.messages;
        } else {
          this.messages = [...this.messages, { id: ++msgId, role: 'assistant', content: data.content, streaming: true }];
        }
        this.scrollBottom();
        break;
      }

      case 'done': {
        const last = this.messages[this.messages.length - 1];
        if (last && last.role === 'assistant') {
          last.streaming = false;
          this.messages = this.messages;
          this.history = [...this.history, { role: 'assistant', content: last.content }];
          this.persistEvent('assistant', last.content, 'text');
        }
        this.isThinking = false;
        this.scrollBottom();
        break;
      }

      case 'ask_self': {
        this.messages = [...this.messages, { id: ++msgId, role: 'ask_self', content: data.content }];
        this.history = [...this.history, { role: 'assistant', content: data.content }];
        this.persistEvent('assistant', data.content, 'ask_self');
        this.isThinking = false;
        this.scrollBottom();
        break;
      }

      case 'daemon_started': {
        this.activeDaemonSkillType = data.skill_type;
        this.#currentGroupId = `dg_${Date.now()}_${++msgId}`;
        this.messages = [...this.messages, {
          id: msgId,
          role: 'daemon_block',
          content: data.task,
          skillType: data.skill_type,
          daemonEvents: [],
          streaming: true,
          groupId: this.#currentGroupId
        }];
        this.persistEvent(data.skill_type, data.task, 'daemon_task', { skill_type: data.skill_type }, this.#currentGroupId);
        this.scrollBottom();
        break;
      }

      case 'daemon_event': {
        const last = this.lastDaemonBlock();
        if (last) {
          last.daemonEvents = [...(last.daemonEvents || []), { event_type: data.event_type, payload: data.payload }];
          this.messages = this.messages;
          this.persistEvent('daemon', data.payload?.content ?? '', data.event_type, data.payload, last.groupId ?? this.#currentGroupId);
        }
        if (data.event_type === 'payment_settled') {
          historyState.applyPaymentSettled(data.payload ?? {});
        }
        this.scrollBottom();
        break;
      }

      case 'daemon_done': {
        const last = this.lastDaemonBlock();
        if (last) {
          last.streaming = false;
          this.messages = this.messages;
          this.history = [
            ...this.history,
            { role: 'assistant', content: this.summarizeDaemonGroup(last.content, last.skillType ?? 'task', last.daemonEvents ?? []) }
          ];
        }
        this.#currentGroupId = null;
        this.activeDaemonSkillType = null;
        this.isThinking = false;
        this.scrollBottom();
        break;
      }

      case 'awaiting_confirmation': {
        const last = this.lastDaemonBlock();
        const skillType = last?.skillType ?? this.activeDaemonSkillType ?? '';
        const groupId = last?.groupId ?? this.#currentGroupId;

        if (last) {
          last.daemonEvents = [...(last.daemonEvents ?? []), {
            event_type: 'ask',
            payload: { content: data.content, kind: data.payload?.kind },
            taskId: data.task_id,
            skillType,
            groupId,
            resolved: false,
            confirmationReply: ''
          }];
          this.messages = this.messages;
        } else {
          this.messages = [...this.messages, {
            id: ++msgId,
            role: 'awaiting_confirmation',
            content: data.content,
            taskId: data.task_id,
            confirmationKind: data.payload?.kind,
            skillType,
            confirmationReply: '',
            groupId
          }];
        }

        this.persistEvent('daemon', data.content, 'ask', { content: data.content, kind: data.payload?.kind, task_id: data.task_id }, groupId);
        if (this.currentSession) {
          tauriInvoke('save_pending_confirmation', {
            sessionId: this.currentSession,
            taskId: data.task_id,
            content: data.content,
            kind: data.payload?.kind,
            skillType
          }).catch(() => {});
        }
        this.isThinking = false;
        this.scrollBottom();
        break;
      }

      case 'error': {
        this.messages = [...this.messages, { id: ++msgId, role: 'error', content: data.message }];
        this.persistEvent('system', data.message, 'error', null, this.#currentGroupId);
        this.isThinking = false;
        this.scrollBottom();
        break;
      }
    }
  }

  // ── Send Message ───────────────────────────────────────────────────────────

  /** @param {string} text */
  async sendMessage(text) {
    const trimmed = text.trim();
    if (!trimmed || this.isThinking || !daemonState.online) return;

    this.messages = [...this.messages, { id: ++msgId, role: 'user', content: trimmed }];
    this.history = [...this.history, { role: 'user', content: trimmed }];
    this.isThinking = true;

    this.persistEvent('user', trimmed, 'text');
    this.scrollBottom();

    try {
      await tauriInvoke('send_message', { history: this.history });
    } catch (err) {
      this.messages = [...this.messages, { id: ++msgId, role: 'error', content: `Failed to reach agent: ${err}` }];
      this.isThinking = false;
    }
  }

  /** @returns {UiMessage | undefined} */
  lastDaemonBlock() {
    return [...this.messages].reverse().find((msg) => msg.role === 'daemon_block');
  }

  /**
   * @param {{ taskId?: string, skillType?: string, groupId?: string | null, kind?: string }} target
   * @param {string} reply
   * @param {(text: string) => void} markResolved
   */
  async answerConfirmation(target, reply, markResolved) {
    const text = reply.trim();
    if (!target.taskId || !target.skillType || !text || !this.currentSession || this.isThinking || !daemonState.online) return;

    markResolved(text);
    this.messages = this.messages;
    this.isThinking = true;

    const groupId = target.groupId ?? this.#currentGroupId;
    this.persistEvent('user', text, 'user_reply', { content: text, task_id: target.taskId, kind: target.kind }, groupId);

    try {
      await tauriInvoke('resume_daemon_task', {
        sessionId: this.currentSession,
        taskId: target.taskId,
        reply: text,
        skillType: target.skillType
      });
    } catch (err) {
      this.messages = [...this.messages, { id: ++msgId, role: 'error', content: `Failed to resume task: ${err}` }];
      this.isThinking = false;
    }
  }

  /**
   * @param {UiMessage} msg
   * @param {string} reply
   */
  async resumeConfirmation(msg, reply) {
    await this.answerConfirmation(
      { taskId: msg.taskId, skillType: msg.skillType, groupId: msg.groupId, kind: msg.confirmationKind },
      reply,
      (text) => { msg.resolved = true; msg.resolutionReply = text; }
    );
  }

  /**
   * @param {DaemonLogEvent} ev
   * @param {string} reply
   */
  async resumeInlineAsk(ev, reply) {
    await this.answerConfirmation(
      { taskId: ev.taskId, skillType: ev.skillType, groupId: ev.groupId, kind: ev.payload?.kind },
      reply,
      (text) => { ev.resolved = true; ev.reply = text; }
    );
  }

  /**
   * Called when a hold is approved or released from the Dashboard tab.
   * Resolves active chat confirmation prompts and updates chat history so the agent knows.
   * @param {'approve' | 'release'} action
   */
  handleDashboardHoldAction(action) {
    for (const msg of this.messages) {
      if (msg.role === 'awaiting_confirmation' && !msg.resolved) {
        msg.resolved = true;
        msg.resolutionReply = action === 'approve' ? 'yes' : 'no';
      }
      if (msg.daemonEvents) {
        for (const ev of msg.daemonEvents) {
          if (ev.event_type === 'ask' && !ev.resolved) {
            ev.resolved = true;
            ev.reply = action === 'approve' ? 'yes' : 'no';
          }
        }
      }
    }
    this.messages = this.messages;

    if (this.currentSession) {
      tauriInvoke('clear_pending_confirmation', { sessionId: this.currentSession }).catch(() => {});
    }

    const note = action === 'approve'
      ? '[System Notice]: Payment hold was approved and executed from the Dashboard.'
      : '[System Notice]: Payment hold was released (cancelled) from the Dashboard.';

    this.history = [
      ...this.history,
      { role: 'user', content: note }
    ];

    if (this.currentSession) {
      this.persistEvent('user', note, 'user_reply', { content: note }, this.#currentGroupId);
    }
  }

  async scrollBottom() {
    setTimeout(() => {
      if (this.messagesEl) this.messagesEl.scrollTop = this.messagesEl.scrollHeight;
    }, 0);
  }
}

export const chatState = new ChatState();
