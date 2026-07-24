<script>
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, onDestroy, tick } from 'svelte';

  /**
   * @typedef {{ role: string, content: string }} ChatHistoryItem
   * @typedef {{ id: string, title: string, ts: number }} Session
   * @typedef {{ event_type: string, payload: Record<string, any> }} DaemonLogEvent
   * @typedef {{
   *   id: number,
   *   role: string,
   *   content: string,
   *   streaming?: boolean,
   *   skillType?: string,
   *   daemonEvents?: DaemonLogEvent[]
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
  let msgId = 0;
  /** @type {ReturnType<typeof setInterval> | undefined} */
  let daemonPollInterval;

  let messagesEl = /** @type {HTMLElement | undefined} */ ($state());
  let unlisten = /** @type {Array<() => void>} */ ([]);

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

    const stored = /** @type {ChatHistoryItem[]} */ (await invoke('load_messages', { sessionId: id }).catch(() => []));
    for (const msg of stored) {
      // Rebuild UI messages (don't show daemon internal events from past turns)
      if (msg.role === 'user' || msg.role === 'assistant') {
        messages = [...messages, { id: ++msgId, role: msg.role, content: msg.content }];
        history = [...history, { role: msg.role, content: msg.content }];
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
          last.streaming = false;
          last.content = data.full_text || last.content;
          messages = messages;
          history = [...history, { role: 'assistant', content: last.content }];
          // Persist assistant reply
          if (currentSession) {
            invoke('save_message', { sessionId: currentSession, role: 'assistant', content: last.content }).catch(() => {});
          }
        }
        isThinking = false;
        scrollBottom();
        break;
      }

      case 'daemon_started': {
        messages = [...messages, {
          id: ++msgId,
          role: 'daemon_block',
          content: data.task,
          skillType: data.skill_type,
          daemonEvents: [],
          streaming: true
        }];
        scrollBottom();
        break;
      }

      case 'daemon_event': {
        const last = messages[messages.length - 1];
        if (last && last.role === 'daemon_block') {
          last.daemonEvents = [...(last.daemonEvents || []), { event_type: data.event_type, payload: data.payload }];
          messages = messages;
        }
        scrollBottom();
        break;
      }

      case 'daemon_done': {
        const last = messages[messages.length - 1];
        if (last && last.role === 'daemon_block') {
          last.streaming = false;
          messages = messages;
        }
        isThinking = false;
        scrollBottom();
        break;
      }

      case 'error': {
        messages = [...messages, { id: ++msgId, role: 'error', content: data.message }];
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
    if (currentSession) {
      invoke('save_message', { sessionId: currentSession, role: 'user', content: text }).catch(() => {});
    }

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
    const labels = /** @type {Record<string, string>} */ ({ thought: '◈', action: '▶', observation: '◉', final: '✔', chat: '◎', error: '✕', done: '■', started: '>' });
    return labels[type] ?? '·';
  }

  /** @param {string} type */
  function evLabel(type) {
    const labels = /** @type {Record<string, string>} */ ({ thought: 'Thought', action: 'Action', observation: 'Result', final: 'Final', chat: 'Reply', error: 'Error', done: 'Done', started: 'Started' });
    return labels[type] ?? type;
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
                    {#if ev.event_type !== 'done'}
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
  {:else}
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
  {/if}
</main>
