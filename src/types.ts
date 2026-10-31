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
  historyRetentionDays: number;
  launchAtLogin: boolean;
  notificationsEnabled: boolean;
  notificationDurationSeconds: number;
  showPromptPreview: boolean;
  theme: ThemePreference;
}

export type ThemePreference = "system" | "light" | "dark";

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

export type AgentClient = "codex" | "claude" | "zcode";
export type ExecutionStatus = Exclude<TaskStatus, "idle">;

export interface TokenUsage {
  input: number | null;
  output: number | null;
  cacheRead: number | null;
  cacheWrite: number | null;
  reasoning: number | null;
  total: number | null;
  inputIncludesCache: boolean;
  outputIncludesReasoning: boolean;
}

export interface ExecutionRecord {
  id: string;
  client: AgentClient;
  sessionId: string;
  turnId: string;
  rootId: string | null;
  parentId: string | null;
  isChild: boolean;
  title: string;
  projectPath: string;
  model: string | null;
  provider: string | null;
  turnIndex: number;
  status: ExecutionStatus;
  attention: AttentionKind;
  startedAt: string | null;
  completedAt: string | null;
  observedAt: string;
  durationMs: number | null;
  activeMs: number | null;
  waitingMs: number | null;
  tokens: TokenUsage;
  live: boolean;
  childCount: number;
}

export interface SourceHealth {
  client: AgentClient;
  state: "available" | "unavailable" | "error";
  observedAt: string | null;
  detail: string;
}

export interface ActivitySummary {
  running: number;
  needsAction: number;
  completedToday: number;
  failedToday: number;
  totalTurns: number;
  tokens: TokenUsage;
  tokenCoverage: { recordedExecutions: number; totalExecutions: number };
}

export interface DailyUsage {
  date: string;
  client: AgentClient;
  turns: number;
  completed: number;
  tokens: TokenUsage;
}

export interface ActivitySnapshot {
  protocolVersion: "activity.v1";
  generatedAt: string;
  executions: ExecutionRecord[];
  summary: ActivitySummary;
  clientSummaries: Record<AgentClient, ActivitySummary>;
  daily: DailyUsage[];
  sources: SourceHealth[];
}

export interface ActivityQuery {
  client?: string;
  status?: string;
  sessionId?: string;
  rootId?: string;
  rootsOnly?: boolean;
  childrenOnly?: boolean;
  since?: string;
  offset?: number;
  limit?: number;
}

export interface ActivityPage {
  executions: ExecutionRecord[];
  total: number;
}

export interface SessionSummary {
  latest: ExecutionRecord;
  turnCount: number;
  childCount: number;
  tokens: TokenUsage;
}

export interface SessionPage {
  sessions: SessionSummary[];
  total: number;
  totalTurns: number;
}

export interface QuotaWindow {
  id: string;
  label: string;
  accountLabel: string;
  provider: string;
  state: "available" | "unavailable" | "unsupported" | "stale" | "error";
  windowMinutes: number | null;
  usedPercent: number | null;
  resetsAt: number | null;
  observedAt: number | null;
  detail: string;
}

export interface QuotaSnapshot {
  generatedAt: string;
  windows: QuotaWindow[];
}

export interface PassportSettings {
  muted: boolean;
  quietStart: number;
  quietEnd: number;
}

export interface PassportState {
  state: "disconnected" | "connecting" | "connected" | "error";
  transport: "usb" | "ble" | null;
  target: string | null;
  deviceName: string | null;
  firmware: string | null;
  lastSyncAt: string | null;
  error: string | null;
  settings: PassportSettings;
}

export interface PassportTarget {
  id: string;
  name: string;
}

export interface AlertRecord {
  id: string;
  taskId: string;
  kind: "completed" | "failed" | "needs_action" | "reset_news";
  title: string;
  createdAt: string;
  unread: boolean;
}

export interface ResetPost {
  id: string;
  url: string;
  text: string;
  publishedAt: number;
  state: "announced" | "confirmed" | "banked";
  timingText: string;
  timingAt: number | null;
  timingKind: "deadline" | "point" | "window" | null;
  timingSourceId: string | null;
  version: string;
}
export interface NewsSettings { enabled: boolean; intervalMinutes: number; }
export interface NewsSnapshot {
  settings: NewsSettings;
  state: string;
  detail: string;
  sourceUrl: string;
  checkedAt: number | null;
  sourceUpdatedAt: number | null;
  nextCheckAt: number;
  baselineAt: number | null;
  posts: ResetPost[];
}

export interface DesktopNotice {
  id: string;
  taskId: string;
  kind: AlertRecord["kind"];
  title: string;
  createdAt: string;
  expiresAt: string;
}
