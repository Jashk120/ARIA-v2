<script>
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { chatState } from '$lib/domains/chat/chatState.svelte.js';

  /**
   * @typedef {import('$lib/types/chat.js').UiMessage} UiMessage
   * @typedef {import('$lib/types/chat.js').DaemonLogEvent} DaemonLogEvent
   */

  /** @type {{ msg: UiMessage }} */
  let { msg } = $props();

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

  /** @param {string | undefined} skill */
  function isPaymentSkill(skill) {
    return typeof skill === 'string' && skill.endsWith('.pay') && skill !== 'query.pay';
  }

  /** @param {string | undefined} content */
  function isPolicyBlockedError(content) {
    return typeof content === 'string' && content.startsWith('Payment blocked by policy');
  }

  /** @param {string | undefined} content */
  function isOpaqueSkillFailure(content) {
    return typeof content === 'string' && content.startsWith('Skill error:');
  }

  /**
   * @param {DaemonLogEvent[]} events
   * @param {number} index
   */
  function precedingActionIsPaymentSkill(events, index) {
    for (let i = index - 1; i >= 0; i--) {
      if (events[i].event_type === 'action') return isPaymentSkill(events[i].payload?.skill);
      if (events[i].event_type === 'ask') return false;
    }
    return false;
  }

  /**
   * Returns true only if no 'ask' event precedes this action — meaning
   * the payment was auto-approved and never surfaced a human confirmation prompt.
   * If there is any 'ask' in the events before this index (whether resolved or
   * pending), the payment went through the manual-confirmation path.
   * @param {DaemonLogEvent[]} events
   * @param {number} index
   */
  function isAutoApprovedPaymentAction(events, index) {
    for (let i = index - 1; i >= 0; i--) {
      if (events[i].event_type === 'ask') return false;
    }
    return true;
  }

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
        {#each msg.daemonEvents ?? [] as ev, evIndex}
          {#if ev.event_type === 'error' && isPolicyBlockedError(ev.payload?.content)}
            <div class="payment-outcome-card payment-blocked-card">
              <span class="payment-outcome-badge">⛔ Policy-blocked</span>
              <pre>{ev.payload?.content}</pre>
              <p class="payment-outcome-note">Rejected automatically — there is nothing to approve.</p>
            </div>
          {:else if ev.event_type === 'observation' && isOpaqueSkillFailure(ev.payload?.content) && precedingActionIsPaymentSkill(msg.daemonEvents ?? [], evIndex)}
            <div class="payment-outcome-card payment-blocked-card">
              <span class="payment-outcome-badge">⛔ Payment failed or blocked</span>
              <pre>{ev.payload?.content}</pre>
              <p class="payment-outcome-note">The daemon doesn't report a specific reason for this payment path — it may be a policy block, a rate limit, or an execution failure. (Known gap — see Known_Issues.md.)</p>
            </div>
          {:else if ev.event_type === 'action' && isPaymentSkill(ev.payload?.skill) && isAutoApprovedPaymentAction(msg.daemonEvents ?? [], evIndex)}
            <div class="payment-outcome-card payment-autoapproved-card">
              <span class="payment-outcome-badge">✓ Auto-approved payment</span>
              <span class="log-content">{ev.payload.skill}{ev.payload.args ? ' — ' + JSON.stringify(ev.payload.args) : ''}</span>
              <p class="payment-outcome-note">Executed without asking — under the auto-approval threshold, or an autonomous payment path (x402).</p>
            </div>
          {:else if ev.event_type === 'ask'}
            <div class="payment-confirmation log-ask-card" class:resolved={ev.resolved}>
              {#if ev.payload?.kind === 'payment' && !ev.resolved}
                <span class="payment-outcome-badge payment-outcome-badge-pending">⏳ Pending approval</span>
              {/if}
              <pre>{ev.payload?.content}</pre>
              {#if ev.resolved}
                <div class="confirmation-status {confirmationStatus(ev.payload?.kind, ev.reply).cls}">
                  {confirmationStatus(ev.payload?.kind, ev.reply).text}
                </div>
              {:else if ev.payload?.kind === 'payment'}
                <div class="confirmation-actions">
                  <button onclick={() => chatState.resumeInlineAsk(ev, 'yes')} disabled={!daemonState.online || chatState.isThinking}>Yes</button>
                  <button onclick={() => chatState.resumeInlineAsk(ev, 'no')} disabled={!daemonState.online || chatState.isThinking}>No</button>
                </div>
                <form
                  class="confirmation-form"
                  onsubmit={(e) => { e.preventDefault(); chatState.resumeInlineAsk(ev, ev.confirmationReply ?? ''); }}
                >
                  <input
                    type="text"
                    bind:value={ev.confirmationReply}
                    disabled={!daemonState.online || chatState.isThinking}
                    placeholder="Something else"
                  />
                  <button type="submit" disabled={!daemonState.online || chatState.isThinking || !(ev.confirmationReply ?? '').trim()}>Submit</button>
                </form>
              {:else}
                <form
                  class="confirmation-form"
                  onsubmit={(e) => { e.preventDefault(); chatState.resumeInlineAsk(ev, ev.confirmationReply ?? ''); }}
                >
                  <input
                    type="text"
                    bind:value={ev.confirmationReply}
                    disabled={!daemonState.online || chatState.isThinking}
                  />
                  <button type="submit" disabled={!daemonState.online || chatState.isThinking || !(ev.confirmationReply ?? '').trim()}>Submit</button>
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
