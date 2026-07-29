<script>
  import DaemonBlock from './DaemonBlock.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { chatState } from '$lib/domains/chat/chatState.svelte.js';
  import { marked } from 'marked';

  /**
   * @typedef {import('$lib/types/chat.js').UiMessage} UiMessage
   */

  /** @type {{ msg: UiMessage }} */
  let { msg } = $props();

  /**
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

{#if msg.role === 'user'}
  <div class="row row-user">
    <div class="bubble bubble-user">{msg.content}</div>
    <div class="avatar avatar-user">YOU</div>
  </div>

{:else if msg.role === 'assistant'}
  <div class="row row-ai">
    <div class="avatar avatar-ai">✦</div>
    <div class="bubble bubble-ai md-body">
      {@html marked.parse(msg.content ?? '', { async: false })}{#if msg.streaming}<span class="caret">█</span>{/if}
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
  <DaemonBlock {msg} />

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
            <button onclick={() => chatState.resumeConfirmation(msg, 'yes')} disabled={!daemonState.online || chatState.isThinking}>Yes</button>
            <button onclick={() => chatState.resumeConfirmation(msg, 'no')} disabled={!daemonState.online || chatState.isThinking}>No</button>
          </div>
          <form
            class="confirmation-form"
            onsubmit={(e) => { e.preventDefault(); chatState.resumeConfirmation(msg, msg.confirmationReply ?? ''); }}
          >
            <input
              type="text"
              bind:value={msg.confirmationReply}
              disabled={!daemonState.online || chatState.isThinking}
              placeholder="Something else"
            />
            <button type="submit" disabled={!daemonState.online || chatState.isThinking || !(msg.confirmationReply ?? '').trim()}>Submit</button>
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
            onsubmit={(e) => { e.preventDefault(); chatState.resumeConfirmation(msg, msg.confirmationReply ?? ''); }}
          >
            <input
              type="text"
              bind:value={msg.confirmationReply}
              disabled={!daemonState.online || chatState.isThinking}
            />
            <button type="submit" disabled={!daemonState.online || chatState.isThinking || !(msg.confirmationReply ?? '').trim()}>Submit</button>
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
