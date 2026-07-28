export interface PaymentRecord {
  transaction_id: string;
  recipient: string;
  amount_hbar: number;
  chain_verified: boolean;
  status?: string;
  hashscan_url?: string;
  skill_called?: string;
  timestamp?: string;
}

export interface HcsRecordItem {
  consensus_timestamp: string;
  sequence_number: number;
  record: Record<string, any> | null;
  decodeError?: boolean;
}
