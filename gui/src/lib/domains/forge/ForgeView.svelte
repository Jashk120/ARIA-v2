<script lang="ts">
  import TopBar from '$lib/domains/shell/TopBar.svelte';

  let skillName = $state('');
  let skillKind = $state('Files');

  type QueueRow = {
    skill: string;
    kind: string;
    requestedBy: string;
    stage: 'Draft' | 'Building' | 'Staged';
    log: string;
  };

  type StagedSkill = {
    skill: string;
    kind: string;
    summary: string;
    decided: 'pending' | 'approved' | 'discarded';
  };

  let queue = $state<QueueRow[]>([
    { skill: 'summarize.pdf', kind: 'Files', requestedBy: 'operator', stage: 'Building', log: 'chunking pdf → embedding 128/240 pages…' },
    { skill: 'translate.hcs', kind: 'Web', requestedBy: 'analyst', stage: 'Draft', log: 'drafting consensus prompt from template…' },
    { skill: 'fetch.web', kind: 'Web', requestedBy: 'operator', stage: 'Building', log: 'crawling allowlisted docs → 42 pages indexed…' },
    { skill: 'pay.x402', kind: 'Payments', requestedBy: 'treasury', stage: 'Draft', log: 'scaffolding payment guardrails + caps check…' }
  ]);

  let staged = $state<StagedSkill[]>([
    { skill: 'summarize.pdf', kind: 'Files', summary: 'Summarizes PDFs locally with page citations.', decided: 'pending' },
    { skill: 'fetch.web', kind: 'Web', summary: 'Fetches allowlisted URLs and extracts clean text.', decided: 'pending' }
  ]);

  function stageBadgeClass(stage: QueueRow['stage']): string {
    if (stage === 'Building') return 'trail-status-badge trail-status-pending';
    if (stage === 'Staged') return 'trail-status-badge trail-status-success';
    return 'trail-status-badge trail-status-unknown';
  }

  function forgeSkill() {
    const name = skillName.trim();
    if (!name) return;
    if (queue.some((r) => r.skill === name) || staged.some((s) => s.skill === name)) return;
    queue = [{ skill: name, kind: skillKind, requestedBy: 'operator', stage: 'Draft', log: 'queued locally — drafting skill scaffold…' }, ...queue];
    skillName = '';
  }

  function approveSkill(skill: string) {
    staged = staged.map((s) => (s.skill === skill ? { ...s, decided: 'approved' } : s));
  }

  function discardSkill(skill: string) {
    staged = staged.filter((s) => s.skill !== skill);
  }
</script>

<section class="chat-panel">
  <TopBar title="Skill Forge" />

  <div class="dashboard-panel">
    <div class="dash-section">
      <h2>Forge a new skill</h2>
      <p class="dash-caps-note" style="margin-bottom: 0.85rem;">
        AGENT FORGES NEW SKILLS — describe a skill and the forge drafts it locally.
      </p>
      <form
        class="settings-add-form"
        onsubmit={(e) => {
          e.preventDefault();
          forgeSkill();
        }}
      >
        <input type="text" placeholder="e.g. summarize.pdf" bind:value={skillName} aria-label="Skill name" />
        <select bind:value={skillKind} aria-label="Skill kind">
          <option>Files</option>
          <option>Web</option>
          <option>Payments</option>
        </select>
        <button type="submit" disabled={!skillName.trim()}>Forge</button>
      </form>
      <div class="chips" style="justify-content: flex-start;">
        <span class="chip">{skillKind}</span>
        <span class="chip">{queue.length} in queue</span>
        <span class="chip">{staged.filter((s) => s.decided === 'pending').length} staged</span>
      </div>
    </div>

    <div class="dash-section">
      <h2>Forging queue</h2>
      <table class="dash-table">
        <thead>
          <tr>
            <th>Skill</th>
            <th>Requested by</th>
            <th>Stage</th>
            <th>Build log excerpt</th>
          </tr>
        </thead>
        <tbody>
          {#each queue as row (row.skill)}
            <tr>
              <td>{row.skill} <span class="trail-status-badge trail-status-unknown">{row.kind}</span></td>
              <td>{row.requestedBy}</td>
              <td><span class={stageBadgeClass(row.stage)}>{row.stage}</span></td>
              <td>{row.log}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>

    <div class="dash-section">
      <h2>Staged skills awaiting approval</h2>
      {#if staged.length === 0}
        <p class="dash-caps-note">QUEUE EMPTY — NOTHING STAGED RIGHT NOW.</p>
      {:else}
        <table class="dash-table">
          <thead>
            <tr>
              <th>Skill</th>
              <th>Summary</th>
              <th>Decision</th>
            </tr>
          </thead>
          <tbody>
            {#each staged as item (item.skill)}
              <tr>
                <td>{item.skill} <span class="trail-status-badge trail-status-unknown">{item.kind}</span></td>
                <td>{item.summary}</td>
                <td>
                  {#if item.decided === 'approved'}
                    <span class="trail-status-badge trail-status-success">✓ Approved</span>
                  {:else}
                    <div class="confirmation-actions" style="margin-bottom: 0;">
                      <button onclick={() => approveSkill(item.skill)}>Approve</button>
                      <button onclick={() => discardSkill(item.skill)}>Discard</button>
                    </div>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>

    <p class="dash-caps-note">Dummy screen — forge runs are simulated locally, nothing is sent to the daemon.</p>
  </div>
</section>
