<script>
  import { dashboardState } from '../dashboardState.svelte.js';
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
        <span class="budget-value">{dashboardState.dashboardBudget.committed_spend_24h.toFixed(4)} ℏ</span>
      </div>
      <div class="budget-stat">
        <span class="budget-label">Held</span>
        <span class="budget-value">{dashboardState.dashboardBudget.held_spend.toFixed(4)} ℏ</span>
      </div>
      <div class="budget-stat">
        <span class="budget-label">Remaining</span>
        <span class="budget-value">
          {dashboardState.dashboardBudget.remaining_budget === null ? 'Unlimited' : `${dashboardState.dashboardBudget.remaining_budget.toFixed(4)} ℏ`}
        </span>
      </div>
      <div class="budget-stat">
        <span class="budget-label">Day cap</span>
        <span class="budget-value">
          {dashboardState.dashboardBudget.per_day_cap === null ? 'None' : `${dashboardState.dashboardBudget.per_day_cap.toFixed(4)} ℏ`}
        </span>
      </div>
    </div>
    <p class="dash-caps-note">
      Per-task cap: {dashboardState.dashboardBudget.per_task_cap === null ? 'None' : `${dashboardState.dashboardBudget.per_task_cap.toFixed(4)} ℏ`}
      &nbsp;·&nbsp;
      Per-day cap: {dashboardState.dashboardBudget.per_day_cap === null ? 'None' : `${dashboardState.dashboardBudget.per_day_cap.toFixed(4)} ℏ`}
      &nbsp;(config-only — not editable here)
    </p>
  {/if}
</section>
