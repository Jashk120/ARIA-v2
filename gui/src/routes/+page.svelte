<script>
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, onDestroy, tick } from 'svelte';

  /**
   * @typedef {{ role: string, content: string }} ChatHistoryItem
   * @typedef {{ id: string, title: string, ts: number }} Session
   * @typedef {{
   *   event_type: string,
   *   payload: Record<string, any>,
   *   resolved?: boolean,
   *   reply?: string,
   *   taskId?: string,
   *   skillType?: string,
   *   groupId?: string | null,
   *   confirmationReply?: string
   * }} DaemonLogEvent
   * @typedef {{
   *   id: number,
   *   role: string,
   *   content: string,
   *   streaming?: boolean,
   *   skillType?: string,
   *   taskId?: string,
   *   confirmationKind?: string,
   *   confirmationReply?: string,
   *   resolved?: boolean,
   *   resolutionReply?: string,
   *   daemonEvents?: DaemonLogEvent[],
   *   groupId?: string | null
   * }} UiMessage
   */

  // ── State ──────────────────────────────────────────────────────────────────

  let messages = /** @type {UiMessage[]} */ ($state([]));
  let history = /** @type {ChatHistoryItem[]} */ ($state([]));
  let sessions = /** @type {Session[]} */ ($state([]));
  let currentSession = /** @type {string | null} */ ($state(null));
  let activeTab = $state('chat');
  let daemonOnline = $state(false);
  let isThinking = $state(false);
  let input = $state('');
  let directType = $state('fs');
  let directTask = $state('');
  let directEvents = /** @type {DaemonLogEvent[]} */ ($state([]));
  let isDirectSending = $state(false);

  /**
   * @typedef {{
   *   per_task_cap: number | null,
   *   per_day_cap: number | null,
   *   committed_spend_24h: number,
   *   held_spend: number,
   *   remaining_budget: number | null
   * }} BudgetInfo
   * @typedef {{ payment_key: string, amount_hbar: number, timestamp: string }} HoldRecord
   */
  let dashboardBudget = /** @type {BudgetInfo | null} */ ($state(null));
  let dashboardHolds = /** @type {HoldRecord[] | null} */ ($state(null));
  let dashboardAllowlist = /** @type {string[] | null} */ ($state(null));
  let dashboardWallet = /** @type {{ account_id: string, balance_hbar: number } | null} */ ($state(null));
  let dashboardLoading = $state(false);
  /** @type {Record<string, string>} */
  let dashboardErrors = $state({});
  let dashboardLoadedOnce = $state(false);
  let activeDaemonSkillType = /** @type {string | null} */ ($state(null));
  let msgId = 0;
  /** Groups every event of the in-flight delegated task under one id so it
   * can be persisted and later reconstructed as a single daemon_block. */
  let currentGroupId = /** @type {string | null} */ (null);
  /** @type {ReturnType<typeof setInterval> | undefined} */
  let daemonPollInterval;

  let messagesEl = /** @type {HTMLElement | undefined} */ ($state());
  let unlisten = /** @type {Array<() => void>} */ ([]);

  // ── Persistence helpers ───────────────────────────────────────────────────

  /**
   * Save any event to disk — the fix for the GUI only ever persisting plain
   * user/assistant text and silently dropping every delegated-task event
   * (thought/action/observation/final/chat/ask). Everything now goes
   * through this one path so a reload can rebuild it.
   * @param {string} role
   * @param {string} content
   * @param {string} eventType
   * @param {Record<string, any> | null} [payload]
   * @param {string | null} [groupId]
   */
  function persistEvent(role, content, eventType, payload = null, groupId = null) {
    if (!currentSession) return;
    invoke('save_event', {
      sessionId: currentSession,
      role,
      content,
      eventType,
      payloadJson: payload ? JSON.stringify(payload) : null,
      groupId
    }).catch(() => {});
  }

  /**
   * Build the one-line context summary the router LLM sees for a completed
   * delegated task, so it retains continuity across turns (and reloads)
   * without replaying every raw thought/action.
   * @param {string} task
   * @param {string} skillType
   * @param {DaemonLogEvent[]} events
   */
  function summarizeDaemonGroup(task, skillType, events) {
    const outcome = [...events].reverse().find(ev => ev.event_type === 'final' || ev.event_type === 'chat');
    const errorEv = events.find(ev => ev.event_type === 'error');
    const result = outcome?.payload?.content ?? (errorEv ? `Error: ${errorEv.payload?.content}` : '(no result)');
    return `Delegated task (${skillType}): ${task}\nResult: ${result}`;
  }

  // ── Lifecycle ──────────────────────────────────────────────────────────────

  onMount(async () => {
    const off = await listen('aria-event', (e) => handleAriaEvent(e.payload));
    unlisten.push(off);
    const offDirect = await listen('direct-daemon-event', (e) => handleDirectEvent(e.payload));
    unlisten.push(offDirect);

    // Poll daemon status every 3 seconds
    await pollDaemon();
    daemonPollInterval = setInterval(pollDaemon, 3000);

    // Load sessions from DB
    await refreshSessions();

    // Auto-open last session or create a fresh one
    if (sessions.length > 0) {
      await openSession(sessions[0].id);
    } else {
      await newSession();
    }
  });

  onDestroy(() => {
    unlisten.forEach(fn => fn());
    clearInterval(daemonPollInterval);
  });

  // ── Daemon Status ──────────────────────────────────────────────────────────

  async function pollDaemon() {
    daemonOnline = await invoke('check_daemon').catch(() => false);
  }

  // ── Session Management ─────────────────────────────────────────────────────

  async function refreshSessions() {
    const raw = /** @type {[string, string, number][]} */ (await invoke('list_sessions').catch(() => []));
    sessions = raw.map(([id, title, ts]) => ({ id, title, ts }));
  }

  async function newSession() {
    const id = `sess_${Date.now()}`;
    await invoke('create_session', { sessionId: id, title: 'New Chat' });
    await refreshSessions();
    await openSession(id);
  }

  /** @param {string} id */
  async function openSession(id) {
    currentSession = id;
    history = [];
    messages = [];
    activeDaemonSkillType = null;

    const stored = /** @type {any[]} */ (await invoke('load_messages', { sessionId: id }).catch(() => []));

    /** @type {Map<string, UiMessage>} */
    const groupBlocks = new Map();

    for (const row of stored) {
      const payload = row.payload_json ? JSON.parse(row.payload_json) : {};

      if (row.group_id) {
        // Part of a delegated-task block — accumulate under its group_id.
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
            messages = [...messages, block];
          }
        } else if (block) {
          // A 'user_reply' that answers an earlier 'ask' in this same group
          // gets folded into that ask entry (resolved + reply) instead of
          // being appended as its own plain log line — so a reload shows
          // the original ask/payment box with its outcome, not two rows.
          const answeredAsk = row.event_type === 'user_reply' && payload.task_id
            ? [...(block.daemonEvents ?? [])].reverse().find(
                (ev) => ev.event_type === 'ask' && ev.payload?.task_id === payload.task_id && !ev.resolved
              )
            : undefined;

          if (answeredAsk) {
            answeredAsk.resolved = true;
            answeredAsk.reply = payload.content;
            messages = messages;
          } else {
            block.daemonEvents = [...(block.daemonEvents ?? []), { event_type: row.event_type, payload }];
            messages = messages;
          }

          if (row.event_type === 'final' || row.event_type === 'chat' || row.event_type === 'error') {
            history = [
              ...history,
              { role: 'assistant', content: summarizeDaemonGroup(block.content, block.skillType ?? 'task', block.daemonEvents ?? []) }
            ];
          }
        }
        continue;
      }

      // Standalone (non-grouped) rows.
      if (row.event_type === 'text') {
        messages = [...messages, { id: ++msgId, role: row.role, content: row.content }];
        history = [...history, { role: row.role, content: row.content }];
      } else if (row.event_type === 'ask_self') {
        messages = [...messages, { id: ++msgId, role: 'ask_self', content: row.content }];
        history = [...history, { role: 'assistant', content: row.content }];
      } else if (row.event_type === 'error') {
        messages = [...messages, { id: ++msgId, role: 'error', content: row.content }];
      }
    }

    const pending = /** @type {{ task_id: string, content: string, kind?: string, skill_type: string } | null} */ (
      await invoke('load_pending_confirmation', { sessionId: id }).catch(() => null)
    );
    if (pending) {
      activeDaemonSkillType = pending.skill_type;
      const block = lastDaemonBlock();
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
        messages = messages;
      } else {
        messages = [...messages, {
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
    await tick();
    scrollBottom();
  }

  /** @param {string} id */
  async function deleteSession(id) {
    await invoke('delete_session', { sessionId: id });
    await refreshSessions();
    if (id === currentSession) {
      if (sessions.length > 0) {
        await openSession(sessions[0].id);
      } else {
        await newSession();
      }
    }
  }

  // ── Event Handling ─────────────────────────────────────────────────────────

  /** @param {any} event */
  function handleAriaEvent(event) {
    const { kind, ...data } = event;

    switch (kind) {
      case 'token': {
        // Thinking → streaming-final is the only transition here: the
        // "thinking" dots render whenever isThinking is true and the last
        // message is still the user's (see the template below); the first
        // token replaces that with a real bubble that keeps growing in
        // place. No separate "final" step — this bubble IS the final answer.
        const last = messages[messages.length - 1];
        if (last && last.role === 'assistant' && last.streaming) {
          last.content += data.content;
          messages = messages;
        } else {
          messages = [...messages, { id: ++msgId, role: 'assistant', content: data.content, streaming: true }];
        }
        scrollBottom();
        break;
      }

      case 'done': {
        const last = messages[messages.length - 1];
        if (last && last.role === 'assistant') {
          // Just stop the caret — content was already built token-by-token
          // above. (Previously this also overwrote content from a
          // separately-assembled full_text, which was redundant and could
          // visibly flash/diverge from what was actually streamed.)
          last.streaming = false;
          messages = messages;
          history = [...history, { role: 'assistant', content: last.content }];
          persistEvent('assistant', last.content, 'text');
        }
        isThinking = false;
        scrollBottom();
        break;
      }

      case 'ask_self': {
        // The router itself asked a clarifying question instead of
        // guessing. Persisted and folded into history exactly like a
        // normal reply so the next turn has full context of what was
        // asked — the conversation just continues normally from here.
        messages = [...messages, { id: ++msgId, role: 'ask_self', content: data.content }];
        history = [...history, { role: 'assistant', content: data.content }];
        persistEvent('assistant', data.content, 'ask_self');
        isThinking = false;
        scrollBottom();
        break;
      }

      case 'daemon_started': {
        activeDaemonSkillType = data.skill_type;
        currentGroupId = `dg_${Date.now()}_${++msgId}`;
        messages = [...messages, {
          id: msgId,
          role: 'daemon_block',
          content: data.task,
          skillType: data.skill_type,
          daemonEvents: [],
          streaming: true,
          groupId: currentGroupId
        }];
        persistEvent(data.skill_type, data.task, 'daemon_task', { skill_type: data.skill_type }, currentGroupId);
        scrollBottom();
        break;
      }

      case 'daemon_event': {
        const last = lastDaemonBlock();
        if (last) {
          last.daemonEvents = [...(last.daemonEvents || []), { event_type: data.event_type, payload: data.payload }];
          messages = messages;
          persistEvent('daemon', data.payload?.content ?? '', data.event_type, data.payload, last.groupId ?? currentGroupId);
        }
        scrollBottom();
        break;
      }

      case 'daemon_done': {
        const last = lastDaemonBlock();
        if (last) {
          last.streaming = false;
          messages = messages;
          // Fold a short summary back into history so the router LLM
          // retains what the daemon actually did/returned on the next
          // turn — previously the router had no memory of this at all.
          history = [
            ...history,
            { role: 'assistant', content: summarizeDaemonGroup(last.content, last.skillType ?? 'task', last.daemonEvents ?? []) }
          ];
        }
        currentGroupId = null;
        activeDaemonSkillType = null;
        isThinking = false;
        scrollBottom();
        break;
      }

      case 'awaiting_confirmation': {
        const last = lastDaemonBlock();
        const skillType = last?.skillType ?? activeDaemonSkillType ?? '';
        const groupId = last?.groupId ?? currentGroupId;

        // Embedded in the daemon block's own event log (same place later
        // thought/action/observation events land) instead of a trailing
        // top-level message — otherwise events that stream in *after* the
        // ask is answered land inside the earlier block while the ask sits
        // after it in the list, making it look chronologically out of order.
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
          messages = messages;
        } else {
          messages = [...messages, {
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

        persistEvent('daemon', data.content, 'ask', { content: data.content, kind: data.payload?.kind, task_id: data.task_id }, groupId);
        if (currentSession) {
          invoke('save_pending_confirmation', {
            sessionId: currentSession,
            taskId: data.task_id,
            content: data.content,
            kind: data.payload?.kind,
            skillType
          }).catch(() => {});
        }
        isThinking = false;
        scrollBottom();
        break;
      }

      case 'error': {
        messages = [...messages, { id: ++msgId, role: 'error', content: data.message }];
        persistEvent('system', data.message, 'error', null, currentGroupId);
        isThinking = false;
        scrollBottom();
        break;
      }
    }
  }

  // ── Send Message ───────────────────────────────────────────────────────────

  async function sendMessage() {
    const text = input.trim();
    if (!text || isThinking || !daemonOnline) return;
    input = '';

    messages = [...messages, { id: ++msgId, role: 'user', content: text }];
    history = [...history, { role: 'user', content: text }];
    isThinking = true;

    // Persist user message
    persistEvent('user', text, 'text');

    await tick();
    scrollBottom();

    try {
      await invoke('send_message', { history });
    } catch (err) {
      messages = [...messages, { id: ++msgId, role: 'error', content: `Failed to reach agent: ${err}` }];
      isThinking = false;
    }
  }

  /** @param {KeyboardEvent} e */
  function handleKeydown(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  }

  async function scrollBottom() {
    await tick();
    if (messagesEl) messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  // ── Dashboard (read-only) ────────────────────────────────────────────────────

  /**
   * Runs the four read-only daemon queries on demand. Called once when the
   * Dashboard tab is first opened and again on manual refresh — no
   * background polling interval.
   */
  async function loadDashboard() {
    if (dashboardLoading) return;
    dashboardLoading = true;
    dashboardLoadedOnce = true;
    /** @type {Record<string, string>} */
    const errors = {};

    const [budget, holds, allowlist, wallet] = await Promise.all([
      invoke('dashboard_query', { query: 'query_budget' }).catch((e) => { errors.budget = String(e); return null; }),
      invoke('dashboard_query', { query: 'query_holds' }).catch((e) => { errors.holds = String(e); return null; }),
      invoke('dashboard_query', { query: 'query_allowlist' }).catch((e) => { errors.allowlist = String(e); return null; }),
      invoke('dashboard_query', { query: 'query_wallet_balance' }).catch((e) => { errors.wallet = String(e); return null; }),
    ]);

    dashboardBudget = /** @type {any} */ (budget);
    dashboardHolds = /** @type {any} */ (holds)?.holds ?? null;
    dashboardAllowlist = /** @type {any} */ (allowlist)?.accounts ?? null;
    dashboardWallet = /** @type {any} */ (wallet);
    dashboardErrors = errors;
    dashboardLoading = false;
  }

  // ── Direct TCP Task ────────────────────────────────────────────────────────

  /** @param {any} event */
  function handleDirectEvent(event) {
    const { kind, ...data } = event;

    switch (kind) {
      case 'started':
        directEvents = [{
          event_type: 'started',
          payload: {
            content: `${data.skill_type}: ${data.task}`
          }
        }];
        break;

      case 'event':
        directEvents = [...directEvents, {
          event_type: data.event_type,
          payload: data.payload
        }];
        break;

      case 'done':
        directEvents = [...directEvents, {
          event_type: 'done',
          payload: { content: 'Done' }
        }];
        isDirectSending = false;
        break;

      case 'error':
        directEvents = [...directEvents, {
          event_type: 'error',
          payload: { content: data.message }
        }];
        isDirectSending = false;
        break;
    }
  }

  async function sendDirectTask() {
    const task = directTask.trim();
    const skill_type = directType.trim();
    if (!task || !skill_type || isDirectSending || !daemonOnline) return;

    isDirectSending = true;
    directEvents = [];

    try {
      await invoke('send_direct_task', { task, skillType: skill_type });
    } catch (err) {
      directEvents = [...directEvents, {
        event_type: 'error',
        payload: { content: `Failed to send task: ${err}` }
      }];
      isDirectSending = false;
    }
  }

  /** @returns {UiMessage | undefined} */
  function lastDaemonBlock() {
    return [...messages].reverse().find(msg => msg.role === 'daemon_block');
  }

  /**
   * Shared core for answering any ask/payment confirmation, whether it's
   * rendered as a standalone message or embedded inline in a daemon
   * block's event log. `markResolved` mutates whichever shape holds the
   * reply so the box updates in place rather than being removed.
   * @param {{ taskId?: string, skillType?: string, groupId?: string | null, kind?: string }} target
   * @param {string} reply
   * @param {(text: string) => void} markResolved
   */
  async function answerConfirmation(target, reply, markResolved) {
    const text = reply.trim();
    if (!target.taskId || !target.skillType || !text || !currentSession || isThinking || !daemonOnline) return;

    // Resolve in place rather than discarding — the box (with its original
    // ask/payment content) stays visible with a clear accepted/declined/
    // replied status, instead of collapsing into a plain "you replied" log
    // line once answered.
    markResolved(text);
    messages = messages;
    isThinking = true;

    // Persisted with the task_id + kind so a reload can re-attach this
    // reply to the exact ask it answered and rebuild the same box.
    const groupId = target.groupId ?? currentGroupId;
    persistEvent('user', text, 'user_reply', { content: text, task_id: target.taskId, kind: target.kind }, groupId);

    try {
      await invoke('resume_daemon_task', {
        sessionId: currentSession,
        taskId: target.taskId,
        reply: text,
        skillType: target.skillType
      });
    } catch (err) {
      messages = [...messages, { id: ++msgId, role: 'error', content: `Failed to resume task: ${err}` }];
      isThinking = false;
    }
  }

  /**
   * Answers a standalone top-level confirmation message (fallback path —
   * used only when a reload's still-pending ask had no matching daemon
   * block to embed into).
   * @param {UiMessage} msg
   * @param {string} reply
   */
  async function resumeConfirmation(msg, reply) {
    await answerConfirmation(
      { taskId: msg.taskId, skillType: msg.skillType, groupId: msg.groupId, kind: msg.confirmationKind },
      reply,
      (text) => { msg.resolved = true; msg.resolutionReply = text; }
    );
  }

  /**
   * Answers an ask embedded inline in a daemon block's event log — the
   * normal live path, so it resolves in exact chronological position
   * alongside whatever streams in next.
   * @param {DaemonLogEvent} ev
   * @param {string} reply
   */
  async function resumeInlineAsk(ev, reply) {
    await answerConfirmation(
      { taskId: ev.taskId, skillType: ev.skillType, groupId: ev.groupId, kind: ev.payload?.kind },
      reply,
      (text) => { ev.resolved = true; ev.reply = text; }
    );
  }

  /** @param {KeyboardEvent} e */
  function handleDirectKeydown(e) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      sendDirectTask();
    }
  }

  // ── Helpers ────────────────────────────────────────────────────────────────

  /** @param {string} type */
  function evIcon(type) {
    const labels = /** @type {Record<string, string>} */ ({ thought: '◈', action: '▶', observation: '◉', final: '✔', chat: '◎', error: '✕', done: '■', started: '>', user_reply: '↳', ask: '?' });
    return labels[type] ?? '·';
  }

  /** @param {string} type */
  function evLabel(type) {
    const labels = /** @type {Record<string, string>} */ ({ thought: 'Thought', action: 'Action', observation: 'Result', final: 'Final', chat: 'Reply', error: 'Error', done: 'Done', started: 'Started', user_reply: 'You replied', ask: 'Asked' });
    return labels[type] ?? type;
  }

  /**
   * Human-readable outcome for a resolved ask/payment confirmation, so an
   * answered box shows a clear accepted/declined/replied status instead of
   * just the raw reply string.
   * @param {string | undefined} kind
   * @param {string | undefined} reply
   */
  function confirmationStatus(kind, reply) {
    if (kind === 'payment') {
      if (reply === 'yes') return { text: '✓ Accepted', cls: 'status-yes' };
      if (reply === 'no') return { text: '✕ Declined', cls: 'status-no' };
      return { text: `Replied: ${reply}`, cls: 'status-other' };
    }
    return { text: `You replied: ${reply}`, cls: 'status-other' };
  }

</script>

<svelte:head><title>ARIA — AI Assistant</title></svelte:head>

<main class="shell">
  <!-- ── Sidebar ──────────────────────────────────────────────────────────── -->
  <aside class="sidebar">
    <div class="sidebar-header">
      <div class="wordmark">
        <span class="wordmark-a">A</span><span class="wordmark-ria">RIA</span>
      </div>
      <p class="wordmark-sub">Autonomous Reasoning &amp;<br>Intelligent Agent</p>
    </div>

    <!-- Daemon status badge -->
    <div class="daemon-badge" class:online={daemonOnline} class:offline={!daemonOnline}>
      <span class="daemon-dot"></span>
      <span class="daemon-label">
        Daemon: {daemonOnline ? 'ONLINE' : 'OFFLINE'}
      </span>
    </div>

    {#if !daemonOnline}
      <p class="daemon-warn">⚠ Start the ARIA daemon to enable AI tasks. Chat is disabled.</p>
    {/if}

    <div class="divider"></div>

    <div class="tabs">
      <button class:active={activeTab === 'chat'} onclick={() => activeTab = 'chat'}>Chatbot</button>
      <button class:active={activeTab === 'direct'} onclick={() => activeTab = 'direct'}>Direct TCP</button>
      <button
        class:active={activeTab === 'dashboard'}
        onclick={() => { activeTab = 'dashboard'; if (!dashboardLoadedOnce) loadDashboard(); }}
      >Dashboard</button>
    </div>

    {#if activeTab === 'chat'}
    <button class="btn-new-chat" onclick={newSession}>
      <span class="btn-icon">+</span> New Chat
    </button>

    <!-- Sessions list -->
    <div class="sessions-list">
      {#each sessions as sess (sess.id)}
        <div
          class="session-item"
          class:active={sess.id === currentSession}
          role="button"
          tabindex="0"
          onclick={() => openSession(sess.id)}
          onkeydown={(e) => e.key === 'Enter' && openSession(sess.id)}
        >
          <span class="session-title">{sess.title}</span>
          <button
            class="session-del"
            onclick={(e) => { e.stopPropagation(); deleteSession(sess.id); }}
            aria-label="Delete session"
          >✕</button>
        </div>
      {/each}
    </div>
    {/if}

    <div class="sidebar-footer">
      <span class="footer-version">v0.1.0</span>
    </div>
  </aside>

  <!-- ── Chat Panel ───────────────────────────────────────────────────────── -->
  {#if activeTab === 'chat'}
    <section class="chat-panel">

    <!-- Top bar -->
    <header class="top-bar">
      <div class="top-bar-left">
        <span class="top-bar-title">
          {sessions.find(s => s.id === currentSession)?.title ?? 'ARIA Chat'}
        </span>
      </div>
      <div class="top-bar-right">
        <div class="status-pill" class:online={daemonOnline}>
          <span class="pulse"></span>
          {daemonOnline ? 'Connected' : 'No Daemon'}
        </div>
      </div>
    </header>

    <!-- Messages -->
    <div class="messages" bind:this={messagesEl}>
      {#if messages.length === 0}
        <div class="empty-state">
          <div class="empty-glyph">✦</div>
          <h1 class="empty-title">What can I help you with?</h1>
          <p class="empty-body">Ask anything. For system tasks like files or web search,<br>make sure the daemon is running.</p>
          <div class="chips">
            <button class="chip" onclick={() => input = 'Search my files for .env files'}>Search files</button>
            <button class="chip" onclick={() => input = 'Explain quantum computing simply'}>Explain something</button>
            <button class="chip" onclick={() => input = 'Look up today\'s Rust news'}>Web search</button>
          </div>
        </div>
      {/if}

      {#each messages as msg (msg.id)}
        {#if msg.role === 'user'}
          <div class="row row-user">
            <div class="bubble bubble-user">{msg.content}</div>
            <div class="avatar avatar-user">YOU</div>
          </div>

        {:else if msg.role === 'assistant'}
          <div class="row row-ai">
            <div class="avatar avatar-ai">✦</div>
            <div class="bubble bubble-ai">
              {msg.content}{#if msg.streaming}<span class="caret">█</span>{/if}
            </div>
          </div>

        {:else if msg.role === 'ask_self'}
          <div class="row row-ai">
            <div class="avatar avatar-ai">?</div>
            <div class="bubble bubble-ai bubble-ask">
              {msg.content}
            </div>
          </div>

        {:else if msg.role === 'daemon_block'}
          <div class="row row-daemon">
            <div class="avatar avatar-daemon">⚙</div>
            <div class="daemon-card">
              <div class="daemon-card-header">
                <span class="skill-badge">{(msg.skillType ?? 'task').toUpperCase()}</span>
                <span class="daemon-task">{msg.content}</span>
                {#if msg.streaming}<span class="spin"></span>{/if}
              </div>
              {#if (msg.daemonEvents?.length ?? 0) > 0}
                <div class="daemon-log">
                  {#each msg.daemonEvents ?? [] as ev}
                    {#if ev.event_type === 'ask'}
                      <!-- Lives in the same event log as everything else so it stays
                           in exact chronological order with events streamed before
                           and after it — whether live or reconstructed on reload. -->
                      <div class="payment-confirmation log-ask-card" class:resolved={ev.resolved}>
                        <pre>{ev.payload?.content}</pre>
                        {#if ev.resolved}
                          <div class="confirmation-status {confirmationStatus(ev.payload?.kind, ev.reply).cls}">
                            {confirmationStatus(ev.payload?.kind, ev.reply).text}
                          </div>
                        {:else if ev.payload?.kind === 'payment'}
                          <div class="confirmation-actions">
                            <button onclick={() => resumeInlineAsk(ev, 'yes')} disabled={!daemonOnline || isThinking}>Yes</button>
                            <button onclick={() => resumeInlineAsk(ev, 'no')} disabled={!daemonOnline || isThinking}>No</button>
                          </div>
                          <form
                            class="confirmation-form"
                            onsubmit={(e) => { e.preventDefault(); resumeInlineAsk(ev, ev.confirmationReply ?? ''); }}
                          >
                            <input
                              type="text"
                              bind:value={ev.confirmationReply}
                              disabled={!daemonOnline || isThinking}
                              placeholder="Something else"
                            />
                            <button type="submit" disabled={!daemonOnline || isThinking || !(ev.confirmationReply ?? '').trim()}>Submit</button>
                          </form>
                        {:else}
                          <form
                            class="confirmation-form"
                            onsubmit={(e) => { e.preventDefault(); resumeInlineAsk(ev, ev.confirmationReply ?? ''); }}
                          >
                            <input
                              type="text"
                              bind:value={ev.confirmationReply}
                              disabled={!daemonOnline || isThinking}
                            />
                            <button type="submit" disabled={!daemonOnline || isThinking || !(ev.confirmationReply ?? '').trim()}>Submit</button>
                          </form>
                        {/if}
                      </div>
                    {:else if ev.event_type !== 'done'}
                      <div class="log-row log-{ev.event_type}">
                        <span class="log-icon">{evIcon(ev.event_type)}</span>
                        <span class="log-type">{evLabel(ev.event_type)}</span>
                        {#if ev.payload?.content}
                          <span class="log-content">{ev.payload.content}</span>
                        {:else if ev.payload?.skill}
                          <span class="log-content">{ev.payload.skill}{ev.payload.args ? ' — ' + JSON.stringify(ev.payload.args) : ''}</span>
                        {/if}
                      </div>
                    {/if}
                  {/each}
                </div>
              {/if}
            </div>
          </div>

        {:else if msg.role === 'awaiting_confirmation'}
          {#if msg.confirmationKind === 'payment'}
            <div class="row row-ai">
              <div class="avatar avatar-ai">✦</div>
              <div class="payment-confirmation" class:resolved={msg.resolved}>
                <pre>{msg.content}</pre>
                {#if msg.resolved}
                  <div class="confirmation-status {confirmationStatus(msg.confirmationKind, msg.resolutionReply).cls}">
                    {confirmationStatus(msg.confirmationKind, msg.resolutionReply).text}
                  </div>
                {:else}
                  <div class="confirmation-actions">
                    <button onclick={() => resumeConfirmation(msg, 'yes')} disabled={!daemonOnline || isThinking}>Yes</button>
                    <button onclick={() => resumeConfirmation(msg, 'no')} disabled={!daemonOnline || isThinking}>No</button>
                  </div>
                  <form
                    class="confirmation-form"
                    onsubmit={(e) => { e.preventDefault(); resumeConfirmation(msg, msg.confirmationReply ?? ''); }}
                  >
                    <input
                      type="text"
                      bind:value={msg.confirmationReply}
                      disabled={!daemonOnline || isThinking}
                      placeholder="Something else"
                    />
                    <button type="submit" disabled={!daemonOnline || isThinking || !(msg.confirmationReply ?? '').trim()}>Submit</button>
                  </form>
                {/if}
              </div>
            </div>
          {:else}
            <div class="row row-ai">
              <div class="avatar avatar-ai">✦</div>
              <div class="bubble bubble-ai bubble-ask" class:resolved={msg.resolved}>
                <div>{msg.content}</div>
                {#if msg.resolved}
                  <div class="confirmation-status {confirmationStatus(msg.confirmationKind, msg.resolutionReply).cls}">
                    {confirmationStatus(msg.confirmationKind, msg.resolutionReply).text}
                  </div>
                {:else}
                  <form
                    class="confirmation-form"
                    onsubmit={(e) => { e.preventDefault(); resumeConfirmation(msg, msg.confirmationReply ?? ''); }}
                  >
                    <input
                      type="text"
                      bind:value={msg.confirmationReply}
                      disabled={!daemonOnline || isThinking}
                    />
                    <button type="submit" disabled={!daemonOnline || isThinking || !(msg.confirmationReply ?? '').trim()}>Submit</button>
                  </form>
                {/if}
              </div>
            </div>
          {/if}

        {:else if msg.role === 'error'}
          <div class="row row-error">
            <div class="error-card">
              <span class="error-icon">⚡</span>
              {msg.content}
            </div>
          </div>
        {/if}
      {/each}

      {#if isThinking && messages[messages.length - 1]?.role === 'user'}
        <div class="row row-ai">
          <div class="avatar avatar-ai">✦</div>
          <div class="bubble bubble-ai thinking">
            <span></span><span></span><span></span>
          </div>
        </div>
      {/if}
    </div>

    <!-- Input zone -->
    <div class="input-zone" class:locked={!daemonOnline}>
      {#if !daemonOnline}
        <div class="input-lock-msg">
          <span>⚠</span> Daemon offline — start the ARIA daemon to use the chat
        </div>
      {/if}
      <div class="input-row">
        <textarea
          id="chat-input"
          class="input-field"
          placeholder={daemonOnline ? 'Message ARIA…' : 'Daemon offline…'}
          rows="1"
          bind:value={input}
          onkeydown={handleKeydown}
          disabled={!daemonOnline || isThinking}
        ></textarea>
        <button
          id="send-btn"
          class="send-btn"
          onclick={sendMessage}
          disabled={!daemonOnline || isThinking || !input.trim()}
          aria-label="Send"
        >
          {#if isThinking}
            <span class="spin-sm"></span>
          {:else}
            ↑
          {/if}
        </button>
      </div>
      <p class="input-hint">Enter to send · Shift+Enter for newline</p>
    </div>
  </section>
  {:else if activeTab === 'direct'}
    <section class="chat-panel">
      <header class="top-bar">
        <div>
          <span>Direct TCP Task</span>
        </div>
        <div>
          {daemonOnline ? 'Connected' : 'No Daemon'}
        </div>
      </header>

      <div class="direct-panel">
        <label>
          Type
          <select bind:value={directType} disabled={!daemonOnline || isDirectSending}>
            <option value="fs">fs</option>
            <option value="web">web</option>
            <option value="os">os</option>
            <option value="other">other</option>
          </select>
        </label>

        <label>
          Task
          <textarea
            bind:value={directTask}
            onkeydown={handleDirectKeydown}
            disabled={!daemonOnline || isDirectSending}
            rows="8"
          ></textarea>
        </label>

        <button
          onclick={sendDirectTask}
          disabled={!daemonOnline || isDirectSending || !directTask.trim()}
        >
          {isDirectSending ? 'Sending...' : 'Send to TCP'}
        </button>

        {#if !daemonOnline}
          <p>Daemon offline. Start the daemon before sending a direct task.</p>
        {/if}

        <h2>Events</h2>
        <div class="direct-log">
          {#if directEvents.length === 0}
            <p>No events yet.</p>
          {:else}
            {#each directEvents as ev}
              <div class="log-row">
                <span class="log-type">{evLabel(ev.event_type)}</span>
                {#if ev.payload?.content}
                  <span class="log-content">{ev.payload.content}</span>
                {:else if ev.payload?.skill}
                  <span class="log-content">{ev.payload.skill}{ev.payload.args ? ' — ' + JSON.stringify(ev.payload.args) : ''}</span>
                {:else}
                  <span class="log-content">{JSON.stringify(ev.payload)}</span>
                {/if}
              </div>
            {/each}
          {/if}
        </div>
      </div>
    </section>
  {:else}
    <section class="chat-panel">
      <header class="top-bar">
        <div>
          <span>Dashboard</span>
        </div>
        <div class="top-bar-right">
          <button onclick={loadDashboard} disabled={!daemonOnline || dashboardLoading}>
            {dashboardLoading ? 'Refreshing…' : '↻ Refresh'}
          </button>
        </div>
      </header>

      <div class="dashboard-panel">
        {#if !daemonOnline}
          <p>Daemon offline. Start the daemon to load dashboard data.</p>
        {:else if !dashboardLoadedOnce}
          <p>Loading…</p>
        {:else}

          <!-- Budget -->
          <section class="dash-section">
            <h2>Budget</h2>
            {#if dashboardErrors.budget}
              <p class="dash-error">⚠ {dashboardErrors.budget}</p>
            {:else if dashboardBudget}
              <div class="budget-bar">
                <div class="budget-stat">
                  <span class="budget-label">Committed (24h)</span>
                  <span class="budget-value">{dashboardBudget.committed_spend_24h.toFixed(4)} ℏ</span>
                </div>
                <div class="budget-stat">
                  <span class="budget-label">Held</span>
                  <span class="budget-value">{dashboardBudget.held_spend.toFixed(4)} ℏ</span>
                </div>
                <div class="budget-stat">
                  <span class="budget-label">Remaining</span>
                  <span class="budget-value">
                    {dashboardBudget.remaining_budget === null ? 'Unlimited' : `${dashboardBudget.remaining_budget.toFixed(4)} ℏ`}
                  </span>
                </div>
                <div class="budget-stat">
                  <span class="budget-label">Day cap</span>
                  <span class="budget-value">
                    {dashboardBudget.per_day_cap === null ? 'None' : `${dashboardBudget.per_day_cap.toFixed(4)} ℏ`}
                  </span>
                </div>
              </div>
              <p class="dash-caps-note">
                Per-task cap: {dashboardBudget.per_task_cap === null ? 'None' : `${dashboardBudget.per_task_cap.toFixed(4)} ℏ`}
                &nbsp;·&nbsp;
                Per-day cap: {dashboardBudget.per_day_cap === null ? 'None' : `${dashboardBudget.per_day_cap.toFixed(4)} ℏ`}
                &nbsp;(config-only — not editable here)
              </p>
            {/if}
          </section>

          <!-- Wallet -->
          <section class="dash-section">
            <h2>Wallet</h2>
            {#if dashboardErrors.wallet}
              <p class="dash-error">⚠ {dashboardErrors.wallet}</p>
            {:else if dashboardWallet}
              <p class="dash-wallet">
                <span class="dash-wallet-account">{dashboardWallet.account_id}</span>
                <span class="dash-wallet-balance">{dashboardWallet.balance_hbar.toFixed(4)} ℏ</span>
              </p>
            {/if}
          </section>

          <!-- Holds -->
          <section class="dash-section">
            <h2>Holds — pending / uncommitted</h2>
            {#if dashboardErrors.holds}
              <p class="dash-error">⚠ {dashboardErrors.holds}</p>
            {:else if dashboardHolds && dashboardHolds.length === 0}
              <p>No holds.</p>
            {:else if dashboardHolds}
              <table class="dash-table">
                <thead>
                  <tr><th>Payment key</th><th>Amount</th><th>Timestamp</th></tr>
                </thead>
                <tbody>
                  {#each dashboardHolds as hold}
                    <tr>
                      <td>{hold.payment_key}</td>
                      <td>{hold.amount_hbar.toFixed(4)} ℏ</td>
                      <td>{hold.timestamp}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            {/if}
          </section>

          <!-- Allowlist -->
          <section class="dash-section">
            <h2>Allowlist</h2>
            {#if dashboardErrors.allowlist}
              <p class="dash-error">⚠ {dashboardErrors.allowlist}</p>
            {:else if dashboardAllowlist && dashboardAllowlist.length === 0}
              <p>No allowlisted accounts.</p>
            {:else if dashboardAllowlist}
              <ul class="dash-list">
                {#each dashboardAllowlist as account}
                  <li>{account}</li>
                {/each}
              </ul>
            {/if}
            <p class="dash-caps-note">Display only — manage entries from Settings.</p>
          </section>

        {/if}
      </div>
    </section>
  {/if}
</main>
