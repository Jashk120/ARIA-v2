<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';

  const routes = [
    { task: 'Chat / General', model: 'gemma-4-31b-it', latency: '180ms', cost: '$', status: 'Active', badge: 'trail-status-badge trail-status-success' },
    { task: 'Code & Files', model: 'qwen2.5-coder:32b', latency: '320ms', cost: '$$', status: 'Active', badge: 'trail-status-badge trail-status-success' },
    { task: 'Web Search Summaries', model: 'deepseek-v4-flash', latency: '950ms', cost: '$$', status: 'Pilot', badge: 'trail-status-badge trail-status-pending' },
    { task: 'Payments / Policy-gated', model: 'google/gemma-4-26b-a4b-it:free', latency: '410ms', cost: '$', status: 'Active', badge: 'trail-status-badge trail-status-success' },
    { task: 'Skill Forge drafts', model: 'deepseek-r1:8b', latency: '1200ms', cost: '$$$', status: 'Draft', badge: 'trail-status-badge trail-status-pending' }
  ];

  const stats = [
    { label: 'Active routes', value: '5' },
    { label: 'Fallbacks configured', value: '3' },
    { label: 'Avg routing overhead', value: '42ms' }
  ];

  const rules = [
    'If task mentions files → qwen2.5-coder:32b',
    'If payment involved → google/gemma-4-26b-a4b-it:free (policy-strict)',
    'If long web summary needed → deepseek-v4-flash',
    'Default everything else → gemma-4-31b-it'
  ];
</script>

<section class="chat-panel">
  <TopBar title="Models" />

  <div class="dashboard-panel">
    <div style="margin-bottom: 1rem;">
      <h1 style="margin: 0;">Model Routing</h1>
      <p class="dash-caps-note">Live defaults from daemon config (Ollama) + OpenRouter free tier</p>
    </div>

    <section class="dash-section">
      <h2>Task → Model map</h2>
      <table class="dash-table">
        <thead>
          <tr>
            <th>Task</th>
            <th>Model</th>
            <th>Latency</th>
            <th>Cost</th>
            <th>Status</th>
          </tr>
        </thead>
        <tbody>
          {#each routes as r}
            <tr>
              <td>{r.task}</td>
              <td>{r.model}</td>
              <td>{r.latency}</td>
              <td>{r.cost}</td>
              <td><span class={r.badge}>{r.status}</span></td>
            </tr>
          {/each}
        </tbody>
      </table>
    </section>

    <section class="dash-section">
      <h2>Routing stats</h2>
      <div class="budget-bar">
        {#each stats as s}
          <div class="budget-stat">
            <span class="budget-label">{s.label}</span>
            <span class="budget-value">{s.value}</span>
          </div>
        {/each}
      </div>
    </section>

    <section class="dash-section">
      <h2>Routing rules</h2>
      <ul class="dash-list">
        {#each rules as rule}
          <li>{rule}</li>
        {/each}
      </ul>
      <p class="dash-caps-note">Preview — IDs match daemon defaults (OLLAMA_MODEL / OpenRouter) and native-capability allowlist; not yet wired to daemon.</p>
    </section>
  </div>
</section>
