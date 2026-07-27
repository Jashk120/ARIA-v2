export interface BudgetInfo {
  per_task_cap: number | null;
  per_day_cap: number | null;
  committed_spend_24h: number;
  held_spend: number;
  remaining_budget: number | null;
}

export interface HoldRecord {
  payment_key: string;
  amount_hbar: number;
  timestamp: string;
}

export interface WalletInfo {
  account_id: string;
  balance_hbar: number;
}
