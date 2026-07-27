<script>
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { chatState } from '$lib/domains/chat/chatState.svelte.js';

  let { activeTab = $bindable('chat') } = $props();
</script>

<aside class="sidebar">
  <div class="sidebar-header">
    <div class="wordmark">
      <span class="wordmark-a">A</span><span class="wordmark-ria">RIA</span>
    </div>
    <p class="wordmark-sub">Autonomous Reasoning &amp;<br>Intelligent Agent</p>
  </div>

  <!-- Daemon status badge -->
  <div class="daemon-badge" class:online={daemonState.online} class:offline={!daemonState.online}>
    <span class="daemon-dot"></span>
    <span class="daemon-label">
      Daemon: {daemonState.online ? 'ONLINE' : 'OFFLINE'}
    </span>
  </div>

  {#if !daemonState.online}
    <p class="daemon-warn">⚠ Start the ARIA daemon to enable AI tasks. Chat is disabled.</p>
  {/if}

  <div class="divider"></div>

  <div class="tabs">
    <button class:active={activeTab === 'chat'} onclick={() => activeTab = 'chat'}>Chatbot</button>
    <button class:active={activeTab === 'direct'} onclick={() => activeTab = 'direct'}>Direct TCP</button>
    <button class:active={activeTab === 'dashboard'} onclick={() => activeTab = 'dashboard'}>Dashboard</button>
    <button class:active={activeTab === 'history'} onclick={() => activeTab = 'history'}>History</button>
    <button class:active={activeTab === 'settings'} onclick={() => activeTab = 'settings'}>Settings</button>
  </div>

  {#if activeTab === 'chat'}
    <button class="btn-new-chat" onclick={() => chatState.newSession()}>
      <span class="btn-icon">+</span> New Chat
    </button>

    <!-- Sessions list -->
    <div class="sessions-list">
      {#each chatState.sessions as sess (sess.id)}
        <div
          class="session-item"
          class:active={sess.id === chatState.currentSession}
          role="button"
          tabindex="0"
          onclick={() => chatState.openSession(sess.id)}
          onkeydown={(e) => e.key === 'Enter' && chatState.openSession(sess.id)}
        >
          <span class="session-title">{sess.title}</span>
          <button
            class="session-del"
            onclick={(e) => { e.stopPropagation(); chatState.deleteSession(sess.id); }}
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
