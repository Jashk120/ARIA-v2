<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import ChatMessage from './components/ChatMessage.svelte';
  import ChatInput from './components/ChatInput.svelte';
  import { chatState } from './chatState.svelte.js';

  let inputComponent = /** @type {ChatInput | undefined} */ ($state());

  /** @param {string} text */
  function setPrompt(text) {
    if (inputComponent) {
      inputComponent.setInputText(text);
    }
  }

  const currentTitle = $derived(
    chatState.sessions.find((s) => s.id === chatState.currentSession)?.title ?? 'ARIA Chat'
  );
</script>

<section class="chat-panel">
  <TopBar title={currentTitle} />

  <!-- Messages -->
  <div class="messages" bind:this={chatState.messagesEl}>
    {#if chatState.messages.length === 0}
      <div class="empty-state">
        <div class="empty-glyph">✦</div>
        <h1 class="empty-title">What can I help you with?</h1>
        <p class="empty-body">Ask anything. For system tasks like files or web search,<br>make sure the daemon is running.</p>
        <div class="chips">
          <button class="chip" onclick={() => setPrompt('Search my files for .env files')}>Search files</button>
          <button class="chip" onclick={() => setPrompt('Explain quantum computing simply')}>Explain something</button>
          <button class="chip" onclick={() => setPrompt("Look up today's Rust news")}>Web search</button>
        </div>
      </div>
    {/if}

    {#each chatState.messages as msg (msg.id)}
      <ChatMessage {msg} />
    {/each}

    {#if chatState.isThinking && chatState.messages[chatState.messages.length - 1]?.role === 'user'}
      <div class="row row-ai">
        <div class="avatar avatar-ai">✦</div>
        <div class="bubble bubble-ai thinking">
          <span></span><span></span><span></span>
        </div>
      </div>
    {/if}
  </div>

  <ChatInput bind:this={inputComponent} />
</section>
