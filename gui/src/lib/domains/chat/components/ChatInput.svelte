<script>
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { chatState } from '$lib/domains/chat/chatState.svelte.js';

  let input = $state('');

  function send() {
    const text = input.trim();
    if (!text) return;
    input = '';
    chatState.sendMessage(text);
  }

  /** @param {KeyboardEvent} e */
  function handleKeydown(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  /** @param {string} text */
  export function setInputText(text) {
    input = text;
  }
</script>

<div class="input-zone" class:locked={!daemonState.online}>
  {#if !daemonState.online}
    <div class="input-lock-msg">
      <span>⚠</span> Daemon offline — start the ARIA daemon to use the chat
    </div>
  {/if}
  <div class="input-row">
    <textarea
      id="chat-input"
      class="input-field"
      placeholder={daemonState.online ? 'Message ARIA…' : 'Daemon offline…'}
      rows="1"
      bind:value={input}
      onkeydown={handleKeydown}
      disabled={!daemonState.online || chatState.isThinking}
    ></textarea>
    <button
      id="send-btn"
      class="send-btn"
      onclick={send}
      disabled={!daemonState.online || chatState.isThinking || !input.trim()}
      aria-label="Send"
    >
      {#if chatState.isThinking}
        <span class="spin-sm"></span>
      {:else}
        ↑
      {/if}
    </button>
  </div>
  <p class="input-hint">Enter to send · Shift+Enter for newline</p>
</div>
