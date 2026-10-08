<script>
  import { dashboardState } from '../dashboardState.svelte.js';

  /**
   * Daemon-shaped numbers may be missing; never let `.toFixed` throw in the template.
   * @param {number | null | undefined} v
   */
  function fmtHbar(v) {
    return Number(v ?? 0).toFixed(4);
  }
</script>

<section class="dash-section">
  <h2>Budget</h2>
  <p class="dash-caps-note">Per-task/per-day caps apply to both hedera_pay and x402_pay.</p>
  {#if dashboardState.dashboardErrors.budget}
    <p class="dash-error">⚠ {dashboardState.dashboardErrors.budget}</p>
  {:else if dashboardState.dashboardBudget}
    <div class="budget-bar">
      <div class="budget-stat">
        <span class="budget-label">Committed (24h)</span>
        <span class="budget-value">{fmtHbar(dashboardState.dashboardBudget.committed_spend_24h)} ℏ</span>
      </div>
      <div class="budget-stat">
        <span class="budget-label">Held</span>
        <span class="budget-value">{fmtHbar(dashboardState.dashboardBudget.held_spend)} ℏ</span>
      </div>
      <div class="budget-stat">
        <span class="budget-label">Remaining</span>
        <span class="budget-value">
          {dashboardState.dashboardBudget.remaining_budget === null ? 'Unlimited' : `${fmtHbar(dashboardState.dashboardBudget.remaining_budget)} ℏ`}
        </span>
      </div>
      <div class="budget-stat">
        <span class="budget-label">Day cap</span>
        <span class="budget-value">
          {dashboardState.dashboardBudget.per_day_cap === null ? 'None' : `${fmtHbar(dashboardState.dashboardBudget.per_day_cap)} ℏ`}
        </span>
      </div>
    </div>
    <p class="dash-caps-note">
      Per-task cap: {dashboardState.dashboardBudget.per_task_cap === null ? 'None' : `${fmtHbar(dashboardState.dashboardBudget.per_task_cap)} ℏ`}
      &nbsp;·&nbsp;
      Per-day cap: {dashboardState.dashboardBudget.per_day_cap === null ? 'None' : `${fmtHbar(dashboardState.dashboardBudget.per_day_cap)} ℏ`}
      &nbsp;(config-only — not editable here)
    </p>
  {:else if dashboardState.dashboardLoading}
    <p class="dash-caps-note">Loading…</p>
  {/if}
</section>
