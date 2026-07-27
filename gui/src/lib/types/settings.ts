export interface RateStatusInfo {
  count: number;
  limit: number;
  window?: string;
}

export type UrlRateStatusMap = Record<string, RateStatusInfo | 'error'>;
