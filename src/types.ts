export type TaskStatus =
  | "running"
  | "needs_action"
  | "idle"
  | "succeeded"
  | "failed"
  | "interrupted"
  | "unknown";

export type AttentionKind = "approval" | "user_input" | null;
export type ProgressBasis = "plan" | "stage" | "none";
export type ProgressConfidence = "high" | "medium" | "low";
export type SyncStatus = "connecting" | "online" | "offline" | "degraded";

export interface TaskSnapshot {
  provider: string;
  taskId: string;
  turnId: string | null;
  parentTaskId: string | null;
  title: string;
  projectPath: string;
  source: string;
  status: TaskStatus;
  attention: AttentionKind;
  progressPercent: number | null;
  progressBasis: ProgressBasis;
  progressConfidence: ProgressConfidence;
  createdAt: string;
  updatedAt: string;
  completedAt: string | null;
  archived: boolean;
  lastError: string | null;
  planTotalSteps: number;
  planCompletedSteps: number;
  planInProgressSteps: number;
}

export interface PlanStep {
  id: string;
  title: string;
  status: string;
}

export interface TurnSummary {
  turnId: string;
  status: string;
  startedAt: string | null;
  completedAt: string | null;
  durationSeconds: number | null;
  planSteps: PlanStep[];
  error: string | null;
}

export interface TaskDetail {
  snapshot: TaskSnapshot;
  recentTurn: TurnSummary | null;
}

export interface ProjectSummary {
  projectPath: string;
  taskCount: number;
  runningCount: number;
  needsActionCount: number;
  idleCount: number;
  succeededCount: number;
  failedCount: number;
  interruptedCount: number;
  estimatedPercent: number | null;
}

export interface SyncState {
  status: SyncStatus;
  lastSyncAt: string | null;
  lastError: string | null;
  compatibilityMode: boolean;
  codexPath: string;
  codexHome: string;
}

export interface DashboardSnapshot {
  protocolVersion: string;
  tasks: TaskSnapshot[];
  projects: ProjectSummary[];
  sync: SyncState;
  generatedAt: string;
}

export interface Settings {
  codexPath: string | null;
  codexHome: string | null;
  refreshIntervalSeconds: number;
  launchAtLogin: boolean;
  notificationsEnabled: boolean;
  showPromptPreview: boolean;
}

export interface Diagnostics {
  codexPath: string;
  codexHome: string;
  codexVersion: string | null;
  connectionStatus: SyncStatus;
  transport: string;
  compatibilityMode: boolean;
  degradationReason: string | null;
}

export interface NotificationEvent {
  eventType: string;
  taskId: string;
  turnId: string | null;
  title: string;
  body: string;
}

export interface AgentAdapterCapabilities {
  list: boolean;
  events: boolean;
  progress: boolean;
  attention: boolean;
  open: boolean;
}

export interface AgentAdapterDescriptor {
  id: string;
  displayName: string;
  capabilities: AgentAdapterCapabilities;
}
