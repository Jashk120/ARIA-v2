<script>
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { chatState } from '$lib/domains/chat/chatState.svelte.js';
  import { marked } from 'marked';

  /**
   * @typedef {import('$lib/types/chat.js').UiMessage} UiMessage
   * @typedef {import('$lib/types/chat.js').DaemonLogEvent} DaemonLogEvent
   */

  /** @type {{ msg: UiMessage }} */
  let { msg } = $props();

  /** Whether the outer tools block is expanded */
  let cardOpen = $state(true);

  /** Per-tool-call open state, keyed by raw event index */
  /** @type {Record<number, boolean>} */
  let openGroups = $state({});

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
  function isAutoApprovedPaymentAction(events, index) {
    for (let i = index - 1; i >= 0; i--) {
      if (events[i].event_type === 'ask') return false;
    }
    return true;
  }

  /** @param {DaemonLogEvent} ev */
  function toolCallLabel(ev) {
    const skill = ev.payload?.skill ?? 'tool';
    const args = ev.payload?.args;
    if (args && typeof args === 'object') {
      const firstVal = Object.values(args).find((v) => typeof v === 'string');
      if (firstVal) {
        const s = /** @type {string} */ (firstVal);
        return `${skill} — ${s.length > 60 ? s.slice(0, 60) + '…' : s}`;
      }
      const raw = JSON.stringify(args);
      return `${skill} — ${raw.length > 60 ? raw.slice(0, 60) + '…' : raw}`;
    }
    return skill;
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

  /**
   * @typedef {{ type: 'tool', key: number, ev: DaemonLogEvent|null, observation: DaemonLogEvent|null, isPaymentAuto: boolean, isPaymentFail: boolean }} ToolGroup
   * @typedef {{ type: 'ask',  key: number, ev: DaemonLogEvent }} AskGroup
   * @typedef {{ type: 'error',key: number, ev: DaemonLogEvent }} ErrorGroup
   */
  const displayGroups = $derived.by(() => {
    const events = msg.daemonEvents ?? [];
    /** @type {Array<ToolGroup | AskGroup | ErrorGroup>} */
    const groups = [];
    let i = 0;
    while (i < events.length) {
      const ev = events[i];
      if (['token','done','final','chat','thought','started','user_reply'].includes(ev.event_type)) {
        i++; continue;
      }
      if (ev.event_type === 'action') {
        let observation = null;
        let nextI = i + 1;
        if (nextI < events.length && events[nextI].event_type === 'observation') {
          observation = events[nextI]; nextI++;
        }
        const isPayment = isPaymentSkill(ev.payload?.skill);
        const isPaymentAuto = isPayment && isAutoApprovedPaymentAction(events, i);
        const isPaymentFail = observation !== null && isOpaqueSkillFailure(observation.payload?.content) && isPayment;
        groups.push({ type: 'tool', key: i, ev, observation, isPaymentAuto, isPaymentFail });
        i = nextI; continue;
      }
      if (ev.event_type === 'observation') {
        groups.push({ type: 'tool', key: i, ev: null, observation: ev, isPaymentAuto: false, isPaymentFail: false });
        i++; continue;
      }
      if (ev.event_type === 'error') { groups.push({ type: 'error', key: i, ev }); i++; continue; }
      if (ev.event_type === 'ask')   { groups.push({ type: 'ask',   key: i, ev }); i++; continue; }
      i++;
    }
    return groups;
  });

  /** Pending ask groups that need user interaction — always visible, never hidden */
  const pendingAsks = $derived(
    displayGroups.filter((g) => g.type === 'ask' && !g.ev.resolved)
  );

  const finalAnswer = $derived.by(() => {
    const events = msg.daemonEvents ?? [];
    for (let i = events.length - 1; i >= 0; i--) {
      if (events[i].event_type === 'final' || events[i].event_type === 'chat') {
        return { text: events[i].payload?.content ?? '', streaming: false };
      }
    }
    const tokens = events.filter((ev) => ev.event_type === 'token');
    if (tokens.length > 0) {
      return { text: tokens.map((ev) => ev.payload?.content ?? '').join(''), streaming: true };
    }
    return null;
  });

  /** Parsed markdown HTML for the final answer */
  const finalHtml = $derived(
    finalAnswer ? /** @type {string} */ (marked.parse(finalAnswer.text, { async: false })) : ''
  );

  // Auto-collapse once done
  $effect(() => {
    if (!msg.streaming && finalAnswer && !finalAnswer.streaming) {
      cardOpen = false;
    }
  });

  /** Label for the outer collapsible toggle */
  const toggleLabel = $derived(
    msg.streaming && !finalAnswer
      ? msg.content
      : `${displayGroups.length} tool call${displayGroups.length !== 1 ? 's' : ''}`
  );
</script>

<!-- Rendered as a standard assistant row so the ✦ avatar and bubble-ai match the rest of chat -->
<div class="row row-ai">
  <div class="avatar avatar-ai">✦</div>
  <div class="daemon-message-body">

    <!-- ── Outer collapsible: one toggle for all tool calls ── -->
    {#if displayGroups.length > 0}
      <div class="tools-block">
        <button
          class="tools-block-toggle"
          onclick={() => (cardOpen = !cardOpen)}
          aria-expanded={cardOpen}
        >
          <span class="tools-block-label">{toggleLabel}</span>
          <span class="tools-chevron" class:open={cardOpen}>
            {#if msg.streaming && !finalAnswer}
              <span class="spin-sm"></span>
            {:else}
              ∨
            {/if}
          </span>
        </button>

        {#if cardOpen}
          <div class="tools-list">
            {#each displayGroups as group (group.key)}

              {#if group.type === 'tool'}
                {#if group.isPaymentAuto}
                  <!-- Auto-approved payment collapsible -->
                  <div class="tool-item">
                    <button
                      class="tool-item-toggle"
                      onclick={() => { openGroups[group.key] = !openGroups[group.key]; }}
                      aria-expanded={!!openGroups[group.key]}
                    >
                      <span class="tool-item-arrow" class:open={!!openGroups[group.key]}>▶</span>
                      <span class="tool-item-name tool-item-payment">✓ {group.ev?.payload?.skill ?? 'payment'}{group.ev?.payload?.args ? ' — ' + (typeof Object.values(group.ev.payload.args)[0] === 'string' ? String(Object.values(group.ev.payload.args)[0]).slice(0,60) : JSON.stringify(group.ev.payload.args).slice(0,60)) : ''}</span>
                      <span class="tool-item-badge tool-item-badge-auto">auto-approved</span>
                    </button>
                    {#if openGroups[group.key]}
                      <div class="tool-item-body">
                        <p class="tool-item-note">Executed without asking — under the auto-approval threshold or x402 path.</p>
                      </div>
                    {/if}
                  </div>

                {:else if group.isPaymentFail}
                  <!-- Payment failure (not collapsible — important to see) -->
                  <div class="tool-item tool-item-error-card">
                    <span class="tool-item-badge tool-item-badge-error">⛔ Payment failed</span>
                    <pre class="tool-item-pre">{group.observation?.payload?.content}</pre>
                  </div>

                {:else}
                  <!-- Normal tool call collapsible -->
                  <div class="tool-item">
                    <button
                      class="tool-item-toggle"
                      onclick={() => { openGroups[group.key] = !openGroups[group.key]; }}
                      aria-expanded={!!openGroups[group.key]}
                    >
                      <span class="tool-item-arrow" class:open={!!openGroups[group.key]}>▶</span>
                      <span class="tool-item-name">
                        {#if group.ev}{toolCallLabel(group.ev)}{:else}Result{/if}
                      </span>
                      {#if !group.observation && msg.streaming}
                        <span class="spin-sm"></span>
                      {:else if group.observation}
                        <span class="tool-item-done">✓</span>
                      {/if}
                    </button>
                    {#if openGroups[group.key] && group.observation}
                      <div class="tool-item-body">
                        <pre class="tool-item-result">{group.observation.payload?.content ?? JSON.stringify(group.observation.payload)}</pre>
                      </div>
                    {/if}
                  </div>
                {/if}

              {:else if group.type === 'error'}
                {#if isPolicyBlockedError(group.ev.payload?.content)}
                  <div class="tool-item tool-item-error-card">
                    <span class="tool-item-badge tool-item-badge-error">⛔ Policy-blocked</span>
                    <pre class="tool-item-pre">{group.ev.payload?.content}</pre>
                  </div>
                {:else}
                  <div class="tool-item-inline-error">
                    <span>✕</span>
                    <span>{group.ev.payload?.content}</span>
                  </div>
                {/if}

              {:else if group.type === 'ask' && group.ev.resolved}
                <!-- Resolved asks shown inside the log -->
                <div class="tool-item tool-item-ask-resolved">
                  <span class="tool-item-name">Ask resolved</span>
                  <span class="confirmation-status {confirmationStatus(group.ev.payload?.kind, group.ev.reply).cls}">
                    {confirmationStatus(group.ev.payload?.kind, group.ev.reply).text}
                  </span>
                </div>
              {/if}

            {/each}
          </div>
        {/if}
      </div>
    {/if}

    <!-- ── Pending asks: always visible, outside collapsible ── -->
    {#each pendingAsks as group (group.key)}
      <div class="payment-confirmation log-ask-card">
        {#if group.ev.payload?.kind === 'payment'}
          <span class="payment-outcome-badge payment-outcome-badge-pending">⏳ Pending approval</span>
        {/if}
        <pre>{group.ev.payload?.content}</pre>
        {#if group.ev.payload?.kind === 'payment'}
          <div class="confirmation-actions">
            <button onclick={() => chatState.resumeInlineAsk(group.ev, 'yes')} disabled={!daemonState.online || chatState.isThinking}>Yes</button>
            <button onclick={() => chatState.resumeInlineAsk(group.ev, 'no')} disabled={!daemonState.online || chatState.isThinking}>No</button>
          </div>
          <form
            class="confirmation-form"
            onsubmit={(e) => { e.preventDefault(); chatState.resumeInlineAsk(group.ev, group.ev.confirmationReply ?? ''); }}
          >
            <input type="text" bind:value={group.ev.confirmationReply} disabled={!daemonState.online || chatState.isThinking} placeholder="Something else" />
            <button type="submit" disabled={!daemonState.online || chatState.isThinking || !(group.ev.confirmationReply ?? '').trim()}>Submit</button>
          </form>
        {:else}
          <form
            class="confirmation-form"
            onsubmit={(e) => { e.preventDefault(); chatState.resumeInlineAsk(group.ev, group.ev.confirmationReply ?? ''); }}
          >
            <input type="text" bind:value={group.ev.confirmationReply} disabled={!daemonState.online || chatState.isThinking} />
            <button type="submit" disabled={!daemonState.online || chatState.isThinking || !(group.ev.confirmationReply ?? '').trim()}>Submit</button>
          </form>
        {/if}
      </div>
    {/each}

    <!-- ── Final answer: rendered markdown, no box ── -->
    {#if finalAnswer}
      <div class="daemon-final-text md-body">
        {@html finalHtml}{#if finalAnswer.streaming}<span class="streaming-caret">█</span>{/if}
      </div>
    {/if}

  </div>
</div>
