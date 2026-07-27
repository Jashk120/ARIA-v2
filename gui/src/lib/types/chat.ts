export interface ChatHistoryItem {
  role: string;
  content: string;
}

export interface Session {
  id: string;
  title: string;
  ts: number;
}

export interface DaemonLogEvent {
  event_type: string;
  payload: Record<string, any>;
  resolved?: boolean;
  reply?: string;
  taskId?: string;
  skillType?: string;
  groupId?: string | null;
  confirmationReply?: string;
}

export interface UiMessage {
  id: number;
  role: string;
  content: string;
  streaming?: boolean;
  skillType?: string;
  taskId?: string;
  confirmationKind?: string;
  confirmationReply?: string;
  resolved?: boolean;
  resolutionReply?: string;
  daemonEvents?: DaemonLogEvent[];
  groupId?: string | null;
}
