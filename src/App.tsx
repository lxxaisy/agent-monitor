import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  AlertCircle,
  Archive,
  ArrowUpRight,
  Bell,
  Check,
  CheckCircle2,
  ChevronDown,
  CircleDashed,
  CircleDot,
  CircleStop,
  Clipboard,
  ExternalLink,
  FolderOpen,
  GitBranch,
  GripHorizontal,
  Info,
  LayoutDashboard,
  LoaderCircle,
  MoveDiagonal2,
  PanelTop,
  PanelRightClose,
  Radar,
  RadioTower,
  RefreshCw,
  Settings as SettingsIcon,
  ShieldCheck,
  TerminalSquare,
  X,
  XCircle,
  Zap,
} from "lucide-react";
import {
  currentWindowLabel,
  hideAppWindow,
  isNativeApp,
  invokeCommand,
  listenEvent,
  showAppWindow,
  startCurrentWindowDragging,
  startCurrentWindowResize,
} from "./lib/tauri";
import type {
  DashboardSnapshot,
  Diagnostics,
  NotificationEvent,
  ProjectSummary,
  Settings,
  SyncStatus,
  TaskDetail,
  TaskSnapshot,
  TaskStatus,
} from "./types";
import "./styles.css";

type View = "overview" | "settings";
type Filter = "live" | "recent_completed" | "all" | "archived" | TaskStatus;

const statusLabels: Record<TaskStatus, string> = {
  running: "运行中",
  needs_action: "待处理",
  idle: "空闲",
  succeeded: "已完成",
  failed: "失败",
  interrupted: "已中断",
  unknown: "未知",
};

const statusTone: Record<TaskStatus, string> = {
  running: "blue",
  needs_action: "amber",
  idle: "muted",
  succeeded: "green",
  failed: "red",
  interrupted: "slate",
  unknown: "muted",
};

const sourceLabels: Record<string, string> = {
  cli: "CLI",
  vscode: "VS Code",
  exec: "Exec",
  appServer: "App Server",
  subAgent: "子 Agent",
  subAgentReview: "Review Agent",
  subAgentCompact: "Compact Agent",
  subAgentThreadSpawn: "子 Agent",
  subAgentOther: "子 Agent",
  unknown: "来源未知",
};

function sourceLabel(source: string): string {
  return sourceLabels[source] ?? source;
}

const syncPresentation: Record<SyncStatus, { quick: string; compact: string; diagnostic: string; tone: string }> = {
  connecting: { quick: "正在连接", compact: "连接中", diagnostic: "连接中", tone: "blue" },
  online: { quick: "实时同步", compact: "已同步", diagnostic: "实时同步", tone: "green" },
  offline: { quick: "暂时离线", compact: "离线", diagnostic: "暂时离线", tone: "red" },
  degraded: { quick: "同步降级", compact: "同步降级", diagnostic: "同步降级 / 兼容模式", tone: "amber" },
};

function StatusIcon({ status, size = 16 }: { status: TaskStatus; size?: number }) {
  if (status === "running") return <LoaderCircle size={size} className="spin" />;
  if (status === "needs_action") return <AlertCircle size={size} />;
  if (status === "succeeded") return <CheckCircle2 size={size} />;
  if (status === "failed") return <XCircle size={size} />;
  if (status === "interrupted") return <CircleStop size={size} />;
  if (status === "idle") return <CircleDashed size={size} />;
  return <Info size={size} />;
}

function relativeTime(value: string): string {
  const timestamp = Date.parse(value);
  if (Number.isNaN(timestamp)) return "时间未知";
  const seconds = Math.max(0, Math.round((Date.now() - timestamp) / 1000));
  if (seconds < 45) return "刚刚";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return `${Math.floor(seconds / 86400)} 天前`;
}

function shortPath(path: string): string {
  if (!path) return "未指定目录";
  const parts = path.split("/").filter(Boolean);
  if (parts.length <= 3) return path;
  return `…/${parts.slice(-2).join("/")}`;
}

function formatDuration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "未记录";
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟`;
  return `${Math.floor(minutes / 60)} 小时 ${minutes % 60} 分钟`;
}

function planStepState(status: string): "done" | "active" | "pending" {
  const normalized = status.replace(/[_-]/g, "").toLowerCase();
  if (["completed", "complete", "done", "finished", "success"].includes(normalized)) return "done";
  if (["inprogress", "running", "active", "doing"].includes(normalized)) return "active";
  return "pending";
}

function errorMessage(caught: unknown): string {
  if (caught instanceof Error && caught.message) return caught.message;
  if (typeof caught === "string" && caught) return caught;
  return "操作失败，请稍后重试";
}

function notificationTone(eventType: string): string {
  if (eventType === "completed") return "green";
  if (eventType === "failed") return "red";
  if (eventType === "interrupted") return "slate";
  return "amber";
}

function isToday(value: string | null): boolean {
  if (value === null) return false;
  const timestamp = Date.parse(value);
  return !Number.isNaN(timestamp) && new Date(timestamp).toDateString() === new Date().toDateString();
}

function isLiveTask(task: TaskSnapshot): boolean {
  return !task.archived && (task.status === "running" || task.status === "needs_action");
}

function isRecentCompletedTask(task: TaskSnapshot): boolean {
  return !task.archived && task.status === "succeeded" && isToday(task.completedAt);
}

function filterLabel(filter: Filter): string {
  if (filter === "live") return "实时";
  if (filter === "recent_completed") return "今日完成";
  if (filter === "all") return "全部记录";
  if (filter === "archived") return "归档";
  return statusLabels[filter];
}

function ProgressBar({ task }: { task: TaskSnapshot }) {
  const value = task.progressPercent;
  return (
    <div className="progress-wrap" aria-label={value === null ? "不可估算" : `估算 ${value}%`}>
      <div className="progress-track">
        {value !== null && <div className={`progress-fill ${statusTone[task.status]} ${task.status === "running" ? "progress-live" : ""}`} style={{ width: `${value}%` }} />}
      </div>
      <span className="progress-value">{value === null ? "不可估算" : `估算${value}%`}</span>
    </div>
  );
}

function Metric({ label, value, tone, detail }: { label: string; value: number; tone?: string; detail?: string }) {
  return (
    <div className={`metric ${tone ?? "neutral"}`} title={detail}>
      <span className="metric-signal" aria-hidden="true"><span /></span>
      <span className="metric-label">{label}</span>
      <strong className={tone ? `metric-value ${tone}` : "metric-value"}>{value}</strong>
    </div>
  );
}

function quickTaskPriority(task: TaskSnapshot): number {
  if (task.status === "needs_action") return 0;
  if (task.status === "running") return 1;
  if (task.status === "succeeded") return 2;
  return 3;
}

function QuickPanel({
  snapshot,
  snapshotError,
  loading,
  syncing,
  onSync,
  onSelect,
  onOpenConsole,
  onHide,
}: {
  snapshot: DashboardSnapshot | null;
  snapshotError: string | null;
  loading: boolean;
  syncing: boolean;
  onSync: () => void;
  onSelect: (taskId: string) => void;
  onOpenConsole: () => void;
  onHide: () => void;
}) {
  const allTasks = snapshot?.tasks ?? [];
  const liveTasks = allTasks.filter(isLiveTask);
  const recentCompletedTasks = allTasks.filter(isRecentCompletedTask);
  const tasks = [...liveTasks, ...recentCompletedTasks];
  const running = liveTasks.filter((task) => task.status === "running").length;
  const needsAction = liveTasks.filter((task) => task.status === "needs_action").length;
  const historyCount = allTasks.filter((task) => !task.archived).length - tasks.length;
  const preview = !isNativeApp();
  const recent = [...tasks]
    .sort((left, right) => quickTaskPriority(left) - quickTaskPriority(right) || Date.parse(right.updatedAt) - Date.parse(left.updatedAt))
    .slice(0, 4);
  const syncStatus = snapshot?.sync.status ?? "connecting";
  const connection = syncPresentation[syncStatus];
  const headline = loading && snapshot === null
    ? "正在读取本机任务"
    : snapshot === null
      ? "无法读取本机任务"
        : syncStatus === "offline"
        ? tasks.length > 0 ? "Codex 暂时离线" : "无法连接 Codex"
        : needsAction > 0
          ? `${needsAction} 个任务需要处理`
          : running > 0
            ? `${running} 个任务正在执行`
            : recentCompletedTasks.length > 0
              ? `${recentCompletedTasks.length} 个任务刚刚完成`
              : "当前没有执行中的任务";
  const updateLabel = loading && snapshot === null
    ? "正在建立本地连接"
    : snapshot === null
      ? "请重试或打开控制台查看诊断"
      : syncStatus === "offline"
        ? snapshot.sync.lastSyncAt ? `显示 ${relativeTime(snapshot.sync.lastSyncAt)} 的快照` : "等待重新连接"
        : syncStatus === "connecting"
          ? snapshot.sync.lastSyncAt ? `显示 ${relativeTime(snapshot.sync.lastSyncAt)} 的快照` : "正在建立本地连接"
          : snapshot.sync.lastSyncAt ? `更新于 ${relativeTime(snapshot.sync.lastSyncAt)}` : "等待首次同步";
  const waitingForFirstSync = snapshot?.sync.status === "connecting" && tasks.length === 0;
  const unavailable = snapshot === null && !loading;
  const offlineWithoutCache = snapshot?.sync.status === "offline" && tasks.length === 0;

  return (
    <div className="quick-shell">
      <button className="quick-drag-handle" onPointerDown={(event) => { if (event.button === 0) { event.preventDefault(); void startCurrentWindowDragging(); } }} aria-label="拖动浮窗" title="拖动浮窗"><GripHorizontal size={18} /></button>
      <header className="quick-header">
        <div className="quick-brand" onPointerDown={(event) => { if (event.button === 0) { event.preventDefault(); void startCurrentWindowDragging(); } }}>
          <span className="quick-brand-mark"><Radar size={16} /></span>
          <div><span className="quick-eyebrow">AGENT MONITOR</span><strong>任务雷达</strong></div>
        </div>
        <div className="quick-actions">
          <button className="quick-icon-button" onClick={onSync} disabled={syncing} aria-label="立即同步" title="立即同步"><RefreshCw size={15} className={syncing ? "spin" : ""} /></button>
          <button className="quick-icon-button" onClick={onHide} aria-label="隐藏浮窗" title="隐藏浮窗"><X size={16} /></button>
        </div>
      </header>

      <section className={`quick-hero ${needsAction > 0 ? "attention" : running > 0 ? "active" : recentCompletedTasks.length > 0 ? "completed" : "quiet"}`}>
        <div className="radar-core" aria-hidden="true">
          <span className="radar-sweep" />
          <span className="radar-cross horizontal" />
          <span className="radar-cross vertical" />
          {running > 0 && <span className="radar-node running" />}
          {needsAction > 0 && <span className="radar-node attention" />}
          {recentCompletedTasks.length > 0 && <span className="radar-node completed" />}
          <span className="radar-center"><CircleDot size={17} /></span>
        </div>
        <div className="quick-hero-copy"><span className="quick-connection"><span className={`quick-connection-dot ${syncStatus}`} />{connection.quick}{preview && <span className="preview-tag">演示数据</span>}</span><strong>{headline}</strong><small>{updateLabel}</small></div>
      </section>

      <div className="quick-stats" aria-label="任务统计">
        <div><strong className="blue">{running}</strong><span>运行中</span></div>
        <div><strong className="amber">{needsAction}</strong><span>待处理</span></div>
        <div><strong className="green">{recentCompletedTasks.length}</strong><span>今日完成</span></div>
      </div>

      <div className="quick-section-heading"><div><span className="quick-section-kicker"><RadioTower size={12} />任务信号</span><strong>{tasks.length ? `${tasks.length} 个关注任务` : "暂无关注任务"}</strong><small className="quick-history-note">{historyCount > 0 ? `历史 ${historyCount} 条已收起` : "只显示执行中与今日完成"}</small></div><button className="quick-open-button" onClick={onOpenConsole}>查看控制台<ArrowUpRight size={14} /></button></div>
      <div className="quick-task-list">
        {loading || waitingForFirstSync ? <div className="quick-empty"><LoaderCircle size={20} className="spin" /><span>正在连接 Codex</span></div> : unavailable ? <div className="quick-empty error"><AlertCircle size={20} /><span>{snapshotError ?? "无法读取任务，请重试"}</span></div> : offlineWithoutCache ? <div className="quick-empty offline"><AlertCircle size={20} /><span>连接恢复后会自动同步</span></div> : recent.length === 0 ? <div className="quick-empty"><Zap size={20} /><span>启动任务后会显示在这里</span></div> : recent.map((task) => (
          <button className={`quick-task ${statusTone[task.status]}`} key={task.taskId} onClick={() => onSelect(task.taskId)}>
            <span className="quick-task-icon"><StatusIcon status={task.status} size={15} /></span>
            <span className="quick-task-copy"><strong title={task.title}>{task.title}</strong><small title={task.projectPath}>{shortPath(task.projectPath)}<span>·</span>{statusLabels[task.status]}</small><span className="quick-progress"><span className="quick-progress-track">{task.progressPercent !== null && <span className={`quick-progress-fill ${statusTone[task.status]} ${task.status === "running" ? "progress-live" : ""}`} style={{ width: `${task.progressPercent}%` }} />}</span><em>{task.progressPercent === null ? "不可估算" : `估算${task.progressPercent}%`}</em></span></span>
            <ArrowUpRight size={14} className="quick-task-arrow" />
          </button>
        ))}
      </div>
      <footer className="quick-footer"><span><span className={`footer-live-dot ${syncStatus}`} />Codex · 本地只读</span><button onClick={onOpenConsole}>控制台 <ArrowUpRight size={12} /></button></footer>
      <button className="quick-resize-grip" onPointerDown={(event) => { if (event.button === 0) { event.preventDefault(); void startCurrentWindowResize("SouthEast"); } }} aria-label="调整浮窗大小" title="调整浮窗大小"><MoveDiagonal2 size={13} /></button>
    </div>
  );
}

function TaskRow({ task, onSelect, nested = false }: { task: TaskSnapshot; onSelect: (id: string) => void; nested?: boolean }) {
  return (
    <button className={`task-row ${nested ? "nested" : ""} status-${task.status}`} onClick={() => onSelect(task.taskId)}>
      <span className={`status-dot ${statusTone[task.status]}`}><StatusIcon status={task.status} size={15} /></span>
      <span className="task-main">
        <span className="task-title-line">
          <span className="task-title">{task.title}</span>
          {task.archived && <span className="archived-badge"><Archive size={10} />归档</span>}
          {task.parentTaskId && <span className="subagent-badge">子任务</span>}
        </span>
        <span className="task-meta">
          <span className="source-mark"><TerminalSquare size={12} /> {sourceLabel(task.source)}</span>
          <span className="meta-separator">·</span>
          <span>{relativeTime(task.updatedAt)}</span>
        </span>
      </span>
      <span className="task-progress"><ProgressBar task={task} /></span>
      <span className={`task-status ${statusTone[task.status]}`}>{statusLabels[task.status]}</span>
      <ChevronDown size={16} className="row-chevron" />
    </button>
  );
}

function ProjectSection({ project, tasks, onSelect }: { project: ProjectSummary; tasks: TaskSnapshot[]; onSelect: (id: string) => void }) {
  const [expanded, setExpanded] = useState(false);
  const taskIds = new Set(tasks.map((task) => task.taskId));
  const roots = tasks.filter((task) => !task.parentTaskId || !taskIds.has(task.parentTaskId));
  const children = tasks.filter((task) => task.parentTaskId && taskIds.has(task.parentTaskId));
  return (
    <section className="project-section">
      <div className="project-header">
        <div className="project-heading">
          <FolderOpen size={16} />
          <span className="project-path" title={project.projectPath}>{shortPath(project.projectPath)}</span>
          <span className="project-count">{project.taskCount} 个任务</span>
          {children.length > 0 && <button className="child-toggle" onClick={() => setExpanded((value) => !value)}><ChevronDown size={13} className={expanded ? "expanded" : ""} />{expanded ? "收起子任务" : `${children.length} 个子任务`}</button>}
        </div>
        <div className="project-summary">
          {project.runningCount > 0 && <span className="summary-chip blue"><Activity size={12} /> {project.runningCount} 运行</span>}
          {project.needsActionCount > 0 && <span className="summary-chip amber"><AlertCircle size={12} /> {project.needsActionCount} 待处理</span>}
          {project.failedCount > 0 && <span className="summary-chip red"><XCircle size={12} /> {project.failedCount} 失败</span>}
          {project.interruptedCount > 0 && <span className="summary-chip slate"><CircleStop size={12} /> {project.interruptedCount} 中断</span>}
          {project.estimatedPercent !== null && <span className="project-estimate">估算 {project.estimatedPercent}%</span>}
        </div>
      </div>
      <div className="task-list">
        {roots.map((task) => <TaskRow key={task.taskId} task={task} onSelect={onSelect} />)}
        {expanded && children.map((task) => <TaskRow key={task.taskId} task={task} onSelect={onSelect} nested />)}
      </div>
    </section>
  );
}

function summarizeVisibleProject(path: string, tasks: TaskSnapshot[]): ProjectSummary {
  const planTotal = tasks.reduce((sum, task) => sum + task.planTotalSteps, 0);
  const planWork = tasks.reduce((sum, task) => sum + task.planCompletedSteps + task.planInProgressSteps * 0.5, 0);
  const activeTasks = tasks.filter((task) => task.status === "running" || task.status === "needs_action");
  const hasUnestimatedActiveTask = activeTasks.some((task) => task.planTotalSteps === 0);
  const estimatedPercent = !hasUnestimatedActiveTask && planTotal > 0
    ? Math.min(activeTasks.length > 0 ? 99 : 100, Math.max(0, Math.round((planWork / planTotal) * 100)))
    : null;
  return {
    projectPath: path,
    taskCount: tasks.length,
    runningCount: tasks.filter((task) => task.status === "running").length,
    needsActionCount: tasks.filter((task) => task.status === "needs_action").length,
    idleCount: tasks.filter((task) => task.status === "idle").length,
    succeededCount: tasks.filter((task) => task.status === "succeeded").length,
    failedCount: tasks.filter((task) => task.status === "failed").length,
    interruptedCount: tasks.filter((task) => task.status === "interrupted").length,
    estimatedPercent,
  };
}

function DetailPanel({ detail, onClose, onOpen }: { detail: TaskDetail; onClose: () => void; onOpen: () => Promise<void> }) {
  const task = detail.snapshot;
  const turn = detail.recentTurn;
  const [copied, setCopied] = useState(false);
  const [opening, setOpening] = useState(false);
  const copyId = async () => {
    try {
      await navigator.clipboard?.writeText(task.taskId);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  };
  return (
    <aside className="detail-panel">
      <div className="detail-topbar">
        <span className="eyebrow">任务详情</span>
        <button className="icon-button" onClick={onClose} aria-label="关闭详情"><PanelRightClose size={18} /></button>
      </div>
      <div className="detail-heading">
        <span className={`detail-status-icon ${statusTone[task.status]}`}><StatusIcon status={task.status} size={18} /></span>
        <div>
          <h2>{task.title}</h2>
          <p title={task.projectPath}>{task.projectPath || "未指定目录"}</p>
        </div>
      </div>
      <div className={`detail-state ${statusTone[task.status]}`}>
        <StatusIcon status={task.status} size={16} />
        <span>{statusLabels[task.status]}</span>
        {task.attention === "approval" && <span className="state-reason">等待批准</span>}
        {task.attention === "user_input" && <span className="state-reason">等待输入</span>}
      </div>
      <div className="detail-block">
        <div className="detail-label">最近 turn</div>
        <div className="detail-grid">
          <span>ID</span><code>{task.turnId ?? "—"}</code>
          <span>来源</span><strong>{sourceLabel(task.source)}</strong>
          <span>状态</span><strong>{turn?.status ?? "—"}</strong>
          <span>运行时长</span><strong>{formatDuration(turn?.durationSeconds)}</strong>
          <span>更新时间</span><strong>{relativeTime(task.updatedAt)}</strong>
        </div>
      </div>
      {turn && turn.planSteps.length > 0 && (
        <div className="detail-block">
          <div className="detail-label plan-label"><span>计划步骤</span><span>{task.planCompletedSteps}/{task.planTotalSteps}</span></div>
          <div className="plan-list">
            {turn.planSteps.map((step) => {
              const state = planStepState(step.status);
              const done = state === "done";
              const active = state === "active";
              return <div className="plan-step" key={step.id}><span className={`plan-check ${done ? "done" : active ? "active" : ""}`}>{done ? <Check size={12} /> : active ? <LoaderCircle size={12} className="spin" /> : <span />}</span><span>{step.title}</span></div>;
            })}
          </div>
        </div>
      )}
      {task.lastError && <div className="error-callout"><AlertCircle size={16} /><div><strong>错误摘要</strong><p>{task.lastError}</p></div></div>}
      <div className="detail-actions">
        <button className="primary-button" disabled={opening} onClick={() => { setOpening(true); void onOpen().finally(() => setOpening(false)); }}>{opening ? <LoaderCircle size={15} className="spin" /> : <ExternalLink size={15} />} {opening ? "正在打开 Codex" : "在 Codex 中打开"}</button>
        <button className="secondary-button" onClick={copyId}>{copied ? <Check size={15} /> : <Clipboard size={15} />} {copied ? "已复制" : "复制任务 ID"}</button>
      </div>
      <div className="detail-footnote"><ShieldCheck size={14} /> 只读监控，不会修改 Codex 任务</div>
    </aside>
  );
}

function SettingsView({ settings, diagnostics, onSave, onClear }: { settings: Settings; diagnostics: Diagnostics | null; onSave: (settings: Settings) => Promise<boolean>; onClear: () => Promise<void> }) {
  const [draft, setDraft] = useState(settings);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  useEffect(() => setDraft(settings), [settings]);
  const save = async () => {
    setSaving(true);
    try {
      if (await onSave(draft)) {
        setSaved(true);
        window.setTimeout(() => setSaved(false), 1600);
      }
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="settings-view">
      <div className="view-heading"><div><span className="eyebrow">偏好设置</span><h1>连接与提醒</h1><p>控制本机 Codex 的读取方式和通知行为。</p></div><button className="primary-button" onClick={save} disabled={saving}>{saving ? <LoaderCircle size={15} className="spin" /> : saved ? <Check size={15} /> : <Check size={15} />}{saved ? "已保存" : "保存设置"}</button></div>
      <div className="settings-grid">
        <section className="settings-section"><div className="section-title"><TerminalSquare size={17} /><div><h2>Codex 连接</h2><p>使用本机 app-server 的 stdio 通道。</p></div></div>
          <label className="field"><span>Codex 可执行文件</span><input value={draft.codexPath ?? ""} placeholder="自动发现 codex" onChange={(event) => setDraft({ ...draft, codexPath: event.target.value || null })} /></label>
          <label className="field"><span>CODEX_HOME</span><input value={draft.codexHome ?? ""} placeholder="自动使用 ~/.codex" onChange={(event) => setDraft({ ...draft, codexHome: event.target.value || null })} /></label>
          <label className="field"><span>刷新间隔</span><select value={draft.refreshIntervalSeconds} onChange={(event) => setDraft({ ...draft, refreshIntervalSeconds: Number(event.target.value) })}><option value={5}>5 秒</option><option value={15}>15 秒</option><option value={30}>30 秒</option><option value={60}>60 秒</option></select></label>
        </section>
        <section className="settings-section"><div className="section-title"><Bell size={17} /><div><h2>通知</h2><p>只提醒 turn 的终态和需要处理的状态。</p></div></div>
          <Toggle label="开启系统通知" checked={draft.notificationsEnabled} onChange={(checked) => setDraft({ ...draft, notificationsEnabled: checked })} />
          <Toggle label="登录后自动启动" checked={draft.launchAtLogin} onChange={(checked) => setDraft({ ...draft, launchAtLogin: checked })} />
          <Toggle label="显示提示词预览" checked={draft.showPromptPreview} onChange={(checked) => setDraft({ ...draft, showPromptPreview: checked })} hint="默认关闭，监控器不会保存完整提示词。" />
        </section>
      </div>
      <section className="diagnostics-section"><div className="section-title"><Activity size={17} /><div><h2>诊断</h2><p>当前连接只展示归一化信息。</p></div></div><div className="diagnostic-list"><Diagnostic label="连接状态" value={diagnostics ? syncPresentation[diagnostics.connectionStatus].diagnostic : "读取中"} tone={diagnostics ? syncPresentation[diagnostics.connectionStatus].tone : undefined} /><Diagnostic label="传输" value={diagnostics?.transport ?? "stdio"} /><Diagnostic label="Codex 版本" value={diagnostics?.codexVersion ?? "未读取"} /><Diagnostic label="兼容模式" value={diagnostics?.compatibilityMode ? "已启用" : "未启用"} tone={diagnostics?.compatibilityMode ? "amber" : "green"} /><Diagnostic label="快照位置" value="~/Library/Application Support/Agent Monitor/monitor.db" /></div>{diagnostics?.degradationReason && <div className="diagnostic-warning"><AlertCircle size={15} /> {diagnostics.degradationReason}</div>}</section>
      <section className="danger-section"><div><h2>清除本地快照</h2><p>只删除 Agent Monitor 自己保存的轻量历史，不会触碰 Codex 目录。</p></div><button className="danger-button" onClick={onClear}><X size={15} /> 清除快照</button></section>
    </div>
  );
}

function Toggle({ label, checked, onChange, hint }: { label: string; checked: boolean; onChange: (checked: boolean) => void; hint?: string }) {
  return <label className="toggle-row"><span><strong>{label}</strong>{hint && <small>{hint}</small>}</span><input type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} /><span className="toggle-ui" /></label>;
}

function Diagnostic({ label, value, tone }: { label: string; value: string; tone?: string }) {
  return <div className="diagnostic"><span>{label}</span><strong className={tone ? tone : ""}>{value}</strong></div>;
}

export default function App() {
  const windowLabel = currentWindowLabel();
  const [view, setView] = useState<View>("overview");
  const [filter, setFilter] = useState<Filter>("live");
  const [snapshot, setSnapshot] = useState<DashboardSnapshot | null>(null);
  const [settings, setSettings] = useState<Settings>({ codexPath: null, codexHome: null, refreshIntervalSeconds: 15, launchAtLogin: true, notificationsEnabled: true, showPromptPreview: false });
  const [diagnostics, setDiagnostics] = useState<Diagnostics | null>(null);
  const [selected, setSelected] = useState<TaskDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [syncing, setSyncing] = useState(false);
  const [toast, setToast] = useState<NotificationEvent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const selectedTaskIdRef = useRef<string | null>(null);
  const toastTimerRef = useRef<number | null>(null);
  const toastKeyRef = useRef<string | null>(null);

  useEffect(() => {
    document.body.classList.toggle("quick-window", windowLabel === "quick");
    document.documentElement.classList.toggle("quick-window", windowLabel === "quick");
    return () => {
      document.body.classList.remove("quick-window");
      document.documentElement.classList.remove("quick-window");
    };
  }, [windowLabel]);

  const refreshSelectedDetail = useCallback(async (taskId: string) => {
    try {
      const detail = await invokeCommand<TaskDetail>("get_task_detail", { taskId });
      setSelected((current) => current?.snapshot.taskId === taskId ? detail : current);
    } catch {
      // Detail refresh is background work. The existing snapshot remains usable.
    }
  }, []);

  const applySnapshot = useCallback((next: DashboardSnapshot) => {
    setSnapshot(next);
    const selectedTaskId = selectedTaskIdRef.current;
    if (!selectedTaskId) return;
    if (next.tasks.some((task) => task.taskId === selectedTaskId)) {
      void refreshSelectedDetail(selectedTaskId);
    } else {
      selectedTaskIdRef.current = null;
      setSelected(null);
    }
  }, [refreshSelectedDetail]);

  const loadDiagnostics = useCallback(async () => {
    try {
      setDiagnostics(await invokeCommand<Diagnostics>("get_diagnostics"));
    } catch (caught) {
      setError(`读取诊断失败：${errorMessage(caught)}`);
    }
  }, []);

  const loadSnapshot = useCallback(async (manual = false) => {
    if (manual) setSyncing(true);
    try {
      const next = await invokeCommand<DashboardSnapshot>(manual ? "sync_now" : "get_dashboard_snapshot");
      applySnapshot(next);
      setSnapshotError(null);
      setError(null);
    } catch (caught) {
      const message = errorMessage(caught);
      setSnapshotError(message);
      setError(message);
    } finally {
      setLoading(false);
      if (manual) setSyncing(false);
    }
  }, [applySnapshot]);

  const showToast = useCallback((event: NotificationEvent) => {
    if (toastTimerRef.current !== null) window.clearTimeout(toastTimerRef.current);
    const key = `${event.eventType}:${event.taskId}:${event.turnId ?? "none"}:${event.title}`;
    toastKeyRef.current = key;
    setToast(event);
    toastTimerRef.current = window.setTimeout(() => {
      if (toastKeyRef.current === key) {
        toastKeyRef.current = null;
        toastTimerRef.current = null;
        setToast(null);
      }
    }, 6000);
  }, []);

  const dismissToast = useCallback(() => {
    if (toastTimerRef.current !== null) window.clearTimeout(toastTimerRef.current);
    toastTimerRef.current = null;
    toastKeyRef.current = null;
    setToast(null);
  }, []);

  useEffect(() => {
    selectedTaskIdRef.current = selected?.snapshot.taskId ?? null;
  }, [selected]);

  useEffect(() => {
    let disposed = false;
    const cleanups: Array<() => void> = [];
    const register = <T,>(event: string, handler: (payload: T) => void) => {
      void listenEvent<T>(event, handler)
        .then((cleanup) => {
          if (disposed) cleanup();
          else cleanups.push(cleanup);
        })
        .catch((caught) => {
          if (!disposed) setError(errorMessage(caught));
        });
    };

    void loadSnapshot();
    void invokeCommand<Settings>("get_settings")
      .then((value) => { if (!disposed) setSettings(value); })
      .catch((caught) => { if (!disposed) setError(`读取设置失败：${errorMessage(caught)}`); });
    void loadDiagnostics();

    register<DashboardSnapshot>("monitor://snapshot-updated", (next) => {
      applySnapshot(next);
    });
    register<NotificationEvent>("monitor://notification", showToast);
    register<{ taskId: string }>("monitor://open-task-detail", ({ taskId }) => {
      if (!taskId) return;
      selectedTaskIdRef.current = taskId;
      void invokeCommand<TaskDetail>("get_task_detail", { taskId })
        .then((detail) => {
          if (selectedTaskIdRef.current === taskId) {
            setSelected(detail);
            setError(null);
          }
        })
        .catch((caught) => {
          if (selectedTaskIdRef.current === taskId) setError(errorMessage(caught));
        });
    });
    register<Settings>("monitor://settings-updated", (next) => {
      setSettings(next);
      void loadDiagnostics();
    });
    register<SyncStateLike>("monitor://connection-changed", (sync) => {
      setSnapshot((current) => current ? { ...current, sync } : current);
      setDiagnostics((current) => current ? {
        ...current,
        codexPath: sync.codexPath,
        codexHome: sync.codexHome,
        connectionStatus: sync.status,
        compatibilityMode: sync.compatibilityMode,
        degradationReason: sync.lastError,
      } : current);
      if (sync.status === "online") {
        setSnapshotError(null);
        setError(null);
      }
    });
    register<{ message: string }>("monitor://sync-error", (payload) => {
      setSnapshotError(payload.message);
      setError(payload.message);
    });

    return () => {
      disposed = true;
      cleanups.splice(0).forEach((cleanup) => cleanup());
    };
  }, [applySnapshot, loadDiagnostics, loadSnapshot, showToast]);

  useEffect(() => {
    const intervalSeconds = Math.max(5, settings.refreshIntervalSeconds);
    const timer = window.setInterval(() => { void loadSnapshot(); }, intervalSeconds * 1000);
    return () => window.clearInterval(timer);
  }, [loadSnapshot, settings.refreshIntervalSeconds]);

  useEffect(() => () => {
    if (toastTimerRef.current !== null) window.clearTimeout(toastTimerRef.current);
  }, []);

  const visibleTasks = useMemo(() => (snapshot?.tasks ?? []).filter((task) => {
    if (filter === "archived") return task.archived;
    if (task.archived) return false;
    if (filter === "live") return isLiveTask(task) || isRecentCompletedTask(task);
    if (filter === "recent_completed") return isRecentCompletedTask(task);
    if (filter === "all") return true;
    return task.status === filter;
  }).sort((left, right) => {
    if (filter === "live") {
      const priority = quickTaskPriority(left) - quickTaskPriority(right);
      if (priority !== 0) return priority;
    }
    return Date.parse(right.updatedAt) - Date.parse(left.updatedAt);
  }), [snapshot, filter]);
  const grouped = useMemo(() => {
    const map = new Map<string, TaskSnapshot[]>();
    visibleTasks.forEach((task) => map.set(task.projectPath, [...(map.get(task.projectPath) ?? []), task]));
    return [...map.entries()];
  }, [visibleTasks]);
  const unarchivedTasks = snapshot?.tasks.filter((task) => !task.archived) ?? [];
  const liveTasks = unarchivedTasks.filter(isLiveTask);
  const recentCompletedTasks = unarchivedTasks.filter(isRecentCompletedTask);
  const stats = {
    running: liveTasks.filter((task) => task.status === "running").length,
    needs: liveTasks.filter((task) => task.status === "needs_action").length,
    completed: recentCompletedTasks.length,
  };
  const syncStatus = snapshot?.sync.status ?? "connecting";
  const overviewSummary = snapshot === null
    ? loading ? "正在读取本机任务" : "无法读取本机任务"
    : syncStatus === "offline"
      ? snapshot.tasks.length > 0
        ? `${liveTasks.length} 个关注任务 · 显示最近快照${snapshot.sync.lastSyncAt ? ` · 更新于 ${relativeTime(snapshot.sync.lastSyncAt)}` : ""}`
        : "Codex 暂时离线"
      : syncStatus === "connecting"
        ? snapshot.tasks.length > 0 ? `${liveTasks.length} 个关注任务 · 正在连接` : "正在连接 Codex"
        : `${stats.running} 个执行中 · ${stats.needs} 个待处理 · ${stats.completed} 个今日完成 · ${snapshot.sync.lastSyncAt ? `更新于 ${relativeTime(snapshot.sync.lastSyncAt)}` : "等待首次同步"}`;
  const waitingForFirstSync = snapshot?.sync.status === "connecting" && snapshot.tasks.length === 0;
  const offlineWithoutCache = snapshot?.sync.status === "offline" && snapshot.tasks.length === 0;

  const selectTask = async (taskId: string) => {
    selectedTaskIdRef.current = taskId;
    try { setSelected(await invokeCommand<TaskDetail>("get_task_detail", { taskId })); } catch (caught) { setError(errorMessage(caught)); }
  };
  const saveSettings = async (next: Settings): Promise<boolean> => {
    try {
      setSettings(await invokeCommand<Settings>("update_settings", { settings: next }));
      await loadSnapshot(true);
      await loadDiagnostics();
      return true;
    } catch (caught) {
      setError(errorMessage(caught));
      return false;
    }
  };
  const openTask = async () => { if (!selected) return; try { await invokeCommand("open_task", { taskId: selected.snapshot.taskId }); } catch (caught) { setError(errorMessage(caught)); } };
  const clearHistory = async () => { if (!window.confirm("确定清除 Agent Monitor 的本地快照吗？Codex 任务不会受到影响。")) return; try { await invokeCommand("clear_local_history"); await loadSnapshot(); setSelected(null); } catch (caught) { setError(errorMessage(caught)); } };
  const openConsole = async () => { try { await showAppWindow("main"); await hideAppWindow("quick"); } catch (caught) { setError(errorMessage(caught)); } };
  const openQuick = async () => { try { await showAppWindow("quick"); await hideAppWindow("main"); } catch (caught) { setError(errorMessage(caught)); } };
  const hideQuick = async () => { try { await hideAppWindow("quick"); } catch (caught) { setError(errorMessage(caught)); } };

  if (windowLabel === "quick") {
    return (
      <>
        <QuickPanel snapshot={snapshot} snapshotError={snapshotError} loading={loading} syncing={syncing} onSync={() => void loadSnapshot(true)} onSelect={(taskId) => { if (taskId) void selectTask(taskId); }} onOpenConsole={() => void openConsole()} onHide={() => void hideQuick()} />
        {selected && <DetailPanel detail={selected} onClose={() => { selectedTaskIdRef.current = null; setSelected(null); }} onOpen={openTask} />}
        {error && <div className="quick-error"><AlertCircle size={14} /><span>{error}</span><button onClick={() => setError(null)} aria-label="关闭提示"><X size={13} /></button></div>}
        {toast && <button className="toast quick-toast" onClick={() => { if (toast.taskId) void selectTask(toast.taskId); dismissToast(); }}><span className={`toast-icon ${notificationTone(toast.eventType)}`}><Bell size={16} /></span><span><strong>{toast.title}</strong><small>{toast.body}</small></span><X size={15} onClick={(event) => { event.stopPropagation(); dismissToast(); }} /></button>}
      </>
    );
  }

  return (
    <div className="app-shell">
      <aside className="side-rail">
        <div className="brand"><span className="brand-mark"><Radar size={17} /></span><span className="brand-copy"><strong>Agent Monitor</strong><small>LOCAL COMMAND</small></span></div>
        <nav className="nav-list"><button className={view === "overview" ? "nav-item active" : "nav-item"} onClick={() => { setView("overview"); setSelected(null); }}><LayoutDashboard size={17} />任务台</button><button className={view === "settings" ? "nav-item active" : "nav-item"} onClick={() => { setView("settings"); setSelected(null); }}><SettingsIcon size={17} />设置与诊断</button></nav>
        <div className="rail-bottom"><div className="provider-label">已连接 Agent</div><div className="provider-row"><span className="provider-icon"><TerminalSquare size={14} /></span><span>Codex</span><span className={`connection-dot ${snapshot?.sync.status ?? "connecting"}`} /></div><div className="privacy-note"><ShieldCheck size={14} /><span>本地只读模式</span></div></div>
      </aside>
      <main className="main-content">
        <header className="topbar"><div className="topbar-title"><span className="eyebrow">Agent Monitor</span><h1>{view === "overview" ? "实时任务" : "设置与诊断"}</h1></div><div className="topbar-actions"><span className={`sync-status ${snapshot?.sync.status ?? "connecting"}`}><span className="sync-dot" />{snapshot ? syncPresentation[snapshot.sync.status].compact : "读取中"}</span><button className="icon-button" onClick={() => void openQuick()} aria-label="打开浮窗" title="打开浮窗"><PanelTop size={17} /></button><button className="icon-button" onClick={() => void loadSnapshot(true)} disabled={syncing} aria-label="立即同步" title="立即同步"><RefreshCw size={17} className={syncing ? "spin" : ""} /></button><button className="icon-button" onClick={() => { setView("settings"); setSelected(null); }} aria-label="设置" title="设置"><SettingsIcon size={17} /></button></div></header>
        {error && <div className="inline-alert"><AlertCircle size={16} /><span>{error}</span><button onClick={() => setError(null)} aria-label="关闭提示"><X size={14} /></button></div>}
        {!isNativeApp() && <div className="preview-banner"><Info size={15} /><span>当前浏览器页面使用演示数据；安装版显示真实 Codex 任务。</span></div>}
        {view === "overview" ? <>
          <section className="overview-heading"><div className="overview-title"><span className="overview-radar" aria-hidden="true"><span /></span><div><h2>实时任务</h2><p>{overviewSummary}</p></div></div><div className="provider-select"><TerminalSquare size={15} /><span>Codex</span></div></section>
          <section className="metrics"><Metric label="正在执行" value={stats.running} tone="blue" detail="最近 turn 正在执行" /><Metric label="等待处理" value={stats.needs} tone="amber" detail="等待批准或用户输入" /><Metric label="今日完成" value={stats.completed} tone="green" detail="今天成功完成的 turn" /></section>
          <div className="filter-row"><div className="segmented">{(["live", "running", "needs_action", "recent_completed", "all", "idle", "succeeded", "failed", "interrupted", "archived"] as Filter[]).map((item) => <button key={item} className={filter === item ? "selected" : ""} aria-pressed={filter === item} onClick={() => setFilter(item)}>{filterLabel(item)}</button>)}</div><div className="filter-meta"><GitBranch size={14} />{filterLabel(filter)} · {visibleTasks.length} 条</div></div>
          {snapshot?.sync.status === "degraded" && <div className="compat-alert"><Info size={15} /><span>{snapshot.sync.lastError ?? (snapshot.sync.compatibilityMode ? "session 文件监听处于兼容模式，任务列表仍由 Codex API 校准。" : "部分同步能力不可用，当前保留可用的任务状态。")}</span></div>}
          {snapshot?.sync.status === "offline" && snapshot.tasks.length > 0 && <div className="offline-alert"><AlertCircle size={15} /><span>Codex 暂时离线，当前显示最近一次本地快照；恢复连接后会自动刷新。</span></div>}
          {(loading && !snapshot) || waitingForFirstSync ? <div className="loading-state"><LoaderCircle size={22} className="spin" /><span>正在读取本机任务…</span></div> : !snapshot ? <div className="empty-state error"><div className="empty-icon"><AlertCircle size={22} /></div><h3>无法读取本机任务</h3><p>{snapshotError ?? "请重试，或打开设置查看诊断。"}</p><button className="secondary-button" onClick={() => void loadSnapshot(true)} disabled={syncing}><RefreshCw size={14} className={syncing ? "spin" : ""} />重新连接</button></div> : offlineWithoutCache ? <div className="empty-state offline"><div className="empty-icon"><AlertCircle size={22} /></div><h3>无法连接 Codex</h3><p>监控器会在后台自动重试，也可以立即重新连接。</p><button className="secondary-button" onClick={() => void loadSnapshot(true)} disabled={syncing}><RefreshCw size={14} className={syncing ? "spin" : ""} />重新连接</button></div> : grouped.length === 0 ? <div className="empty-state"><div className="empty-icon"><FolderOpen size={22} /></div><h3>{snapshot.tasks.length ? filter === "live" ? "当前没有执行中的任务" : "没有匹配的任务" : "还没有可显示的任务"}</h3><p>{snapshot.tasks.length ? filter === "live" ? "运行中的任务和今日完成会显示在这里。" : "调整状态筛选后再试。" : "启动一个 Codex 任务后，它会出现在这里。"}</p></div> : <div className="project-list">{grouped.map(([path, tasks]) => { const project = summarizeVisibleProject(path, tasks); return <ProjectSection key={path || "unknown"} project={project} tasks={tasks} onSelect={(id) => void selectTask(id)} />; })}</div>}
        </> : <SettingsView settings={settings} diagnostics={diagnostics} onSave={saveSettings} onClear={clearHistory} />}
      </main>
      {selected && <DetailPanel detail={selected} onClose={() => { selectedTaskIdRef.current = null; setSelected(null); }} onOpen={openTask} />}
      {toast && <button className="toast" onClick={() => { if (toast.taskId) void selectTask(toast.taskId); dismissToast(); }}><span className={`toast-icon ${notificationTone(toast.eventType)}`}><Bell size={16} /></span><span><strong>{toast.title}</strong><small>{toast.body}</small></span><X size={15} onClick={(event) => { event.stopPropagation(); dismissToast(); }} /></button>}
    </div>
  );
}

type SyncStateLike = DashboardSnapshot["sync"];
