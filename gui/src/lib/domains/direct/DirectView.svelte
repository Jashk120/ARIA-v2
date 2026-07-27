<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { directState } from './directState.svelte.js';
  import { onMount, onDestroy } from 'svelte';

  onMount(() => directState.init());
  onDestroy(() => directState.destroy());

  /** @param {string} type */
  function evLabel(type) {
    const labels = /** @type {Record<string, string>} */ ({ thought: 'Thought', action: 'Action', observation: 'Result', final: 'Final', chat: 'Reply', error: 'Error', done: 'Done', started: 'Started', user_reply: 'You replied', ask: 'Asked' });
    return labels[type] ?? type;
  }

  /** @param {KeyboardEvent} e */
  function handleDirectKeydown(e) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      directState.sendDirectTask();
    }
  }
</script>

<section class="chat-panel">
  <TopBar title="Direct TCP Task" />

  <div class="direct-panel">
    <label>
      Type
      <select bind:value={directState.directType} disabled={!daemonState.online || directState.isDirectSending}>
        <option value="fs">fs</option>
        <option value="web">web</option>
        <option value="os">os</option>
        <option value="other">other</option>
      </select>
    </label>

    <label>
      Task
      <textarea
        bind:value={directState.directTask}
        onkeydown={handleDirectKeydown}
        disabled={!daemonState.online || directState.isDirectSending}
        rows="8"
      ></textarea>
    </label>

    <button
      onclick={() => directState.sendDirectTask()}
      disabled={!daemonState.online || directState.isDirectSending || !directState.directTask.trim()}
    >
      {directState.isDirectSending ? 'Sending...' : 'Send to TCP'}
    </button>

    {#if !daemonState.online}
      <p>Daemon offline. Start the daemon before sending a direct task.</p>
    {/if}

    <h2>Events</h2>
    <div class="direct-log">
      {#if directState.directEvents.length === 0}
        <p>No events yet.</p>
      {:else}
        {#each directState.directEvents as ev}
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
