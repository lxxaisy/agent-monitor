import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getCurrentWindow, type ResizeDirection } from "@tauri-apps/api/window";
import type {
  DashboardSnapshot,
  Diagnostics,
  NotificationEvent,
  Settings,
  TaskDetail,
  TaskSnapshot,
  SyncState,
} from "../types";

const isTauri = () =>
  typeof window !== "undefined" &&
  Boolean((window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__);

export const isNativeApp = isTauri;

export type AppWindowLabel = "main" | "quick";

export function currentWindowLabel(): AppWindowLabel {
  if (isTauri()) {
    try {
      return getCurrentWindow().label === "quick" ? "quick" : "main";
    } catch {
      return "main";
    }
  }
  return new URLSearchParams(window.location.search).get("window") === "quick" ? "quick" : "main";
}

export async function showAppWindow(label: AppWindowLabel): Promise<void> {
  if (!isTauri()) return;
  await tauriInvoke(label === "quick" ? "show_quick" : "show_main");
}

export async function hideAppWindow(label: AppWindowLabel): Promise<void> {
  if (!isTauri()) return;
  const target = await WebviewWindow.getByLabel(label);
  if (target) await target.hide();
}

export async function startCurrentWindowDragging(): Promise<void> {
  if (!isTauri()) return;
  await getCurrentWindow().startDragging();
}

export async function startCurrentWindowResize(direction: ResizeDirection): Promise<void> {
  if (!isTauri()) return;
  await getCurrentWindow().startResizeDragging(direction);
}

export const SAMPLE_TASKS: TaskSnapshot[] = [
  {
    provider: "codex",
    taskId: "00000000-0000-4000-8000-000000000001",
    turnId: "00000000-0000-4000-9000-000000000001",
    parentTaskId: null,
    title: "设计 Codex 任务监控应用",
    projectPath: "/Users/example/Projects/agent-monitor",
    source: "vscode",
    status: "running",
    attention: null,
    progressPercent: 63,
    progressBasis: "plan",
    progressConfidence: "high",
    createdAt: new Date(Date.now() - 1000 * 60 * 48).toISOString(),
    updatedAt: new Date(Date.now() - 1000 * 33).toISOString(),
    completedAt: null,
    archived: false,
    lastError: null,
    planTotalSteps: 8,
    planCompletedSteps: 5,
    planInProgressSteps: 0,
  },
  {
    provider: "codex",
    taskId: "00000000-0000-4000-8000-000000000002",
    turnId: "00000000-0000-4000-9000-000000000002",
    parentTaskId: null,
    title: "完善物流路径规划与国际化资源",
    projectPath: "/Users/example/Projects/route-planner",
    source: "cli",
    status: "needs_action",
    attention: "approval",
    progressPercent: 50,
    progressBasis: "plan",
    progressConfidence: "medium",
    createdAt: new Date(Date.now() - 1000 * 60 * 130).toISOString(),
    updatedAt: new Date(Date.now() - 1000 * 60 * 4).toISOString(),
    completedAt: null,
    archived: false,
    lastError: null,
    planTotalSteps: 6,
    planCompletedSteps: 3,
    planInProgressSteps: 0,
  },
  {
    provider: "codex",
    taskId: "00000000-0000-4000-8000-000000000003",
    turnId: "00000000-0000-4000-9000-000000000003",
    parentTaskId: null,
    title: "验证接口字段并打包前端工程",
    projectPath: "/Users/example/Projects/finance-tools",
    source: "vscode",
    status: "succeeded",
    attention: null,
    progressPercent: 100,
    progressBasis: "plan",
    progressConfidence: "high",
    createdAt: new Date(Date.now() - 1000 * 60 * 280).toISOString(),
    updatedAt: new Date(Date.now() - 1000 * 60 * 12).toISOString(),
    completedAt: new Date(Date.now() - 1000 * 60 * 12).toISOString(),
    archived: false,
    lastError: null,
    planTotalSteps: 4,
    planCompletedSteps: 4,
    planInProgressSteps: 0,
  },
  {
    provider: "codex",
    taskId: "00000000-0000-4000-8000-000000000004",
    turnId: "00000000-0000-4000-9000-000000000004",
    parentTaskId: null,
    title: "演示：API 鉴权失败",
    projectPath: "/Users/example/Projects/route-planner",
    source: "vscode",
    status: "failed",
    attention: null,
    progressPercent: 71,
    progressBasis: "plan",
    progressConfidence: "high",
    createdAt: new Date(Date.now() - 1000 * 60 * 390).toISOString(),
    updatedAt: new Date(Date.now() - 1000 * 60 * 32).toISOString(),
    completedAt: new Date(Date.now() - 1000 * 60 * 32).toISOString(),
    archived: false,
    lastError: "演示故障：上游接口返回 401",
    planTotalSteps: 7,
    planCompletedSteps: 5,
    planInProgressSteps: 0,
  },
  {
    provider: "codex",
    taskId: "00000000-0000-4000-8000-000000000005",
    turnId: "00000000-0000-4000-9000-000000000005",
    parentTaskId: null,
    title: "演示：用户终止执行",
    projectPath: "/Users/example/Projects/demo-app",
    source: "cli",
    status: "interrupted",
    attention: null,
    progressPercent: 29,
    progressBasis: "plan",
    progressConfidence: "high",
    createdAt: new Date(Date.now() - 1000 * 60 * 180).toISOString(),
    updatedAt: new Date(Date.now() - 1000 * 60 * 45).toISOString(),
    completedAt: new Date(Date.now() - 1000 * 60 * 45).toISOString(),
    archived: false,
    lastError: null,
    planTotalSteps: 7,
    planCompletedSteps: 2,
    planInProgressSteps: 0,
  },
  {
    provider: "codex",
    taskId: "00000000-0000-4000-8000-000000000006",
    turnId: "00000000-0000-4000-9000-000000000006",
    parentTaskId: null,
    title: "整理移动端学习流程",
    projectPath: "/Users/example/Projects/language-study",
    source: "cli",
    status: "idle",
    attention: null,
    progressPercent: null,
    progressBasis: "none",
    progressConfidence: "low",
    createdAt: new Date(Date.now() - 1000 * 60 * 800).toISOString(),
    updatedAt: new Date(Date.now() - 1000 * 60 * 95).toISOString(),
    completedAt: null,
    archived: false,
    lastError: null,
    planTotalSteps: 0,
    planCompletedSteps: 0,
    planInProgressSteps: 0,
  },
];

const sampleSync: SyncState = {
  status: "online",
  lastSyncAt: new Date(Date.now() - 1000 * 18).toISOString(),
  lastError: null,
  compatibilityMode: false,
  codexPath: "codex",
  codexHome: "~/.codex",
};

const mockSnapshot = (): DashboardSnapshot => {
  const byPath = new Map<string, TaskSnapshot[]>();
  for (const task of SAMPLE_TASKS) byPath.set(task.projectPath, [...(byPath.get(task.projectPath) ?? []), task]);
  return {
    protocolVersion: "agent-monitor.v1",
    tasks: SAMPLE_TASKS,
    projects: [...byPath.entries()].map(([projectPath, tasks]) => ({
      projectPath,
      taskCount: tasks.length,
      runningCount: tasks.filter((task) => task.status === "running").length,
      needsActionCount: tasks.filter((task) => task.status === "needs_action").length,
      idleCount: tasks.filter((task) => task.status === "idle").length,
      succeededCount: tasks.filter((task) => task.status === "succeeded").length,
      failedCount: tasks.filter((task) => task.status === "failed").length,
      interruptedCount: tasks.filter((task) => task.status === "interrupted").length,
      estimatedPercent: tasks.every((task) => task.planTotalSteps > 0)
        ? Math.round(tasks.reduce((sum, task) => sum + task.planCompletedSteps + task.planInProgressSteps * 0.5, 0) / tasks.reduce((sum, task) => sum + task.planTotalSteps, 0) * 100)
        : null,
    })),
    sync: sampleSync,
    generatedAt: new Date().toISOString(),
  };
};

export async function invokeCommand<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri()) return tauriInvoke<T>(command, args);
  await new Promise((resolve) => setTimeout(resolve, 120));
  if (command === "get_dashboard_snapshot" || command === "sync_now") return mockSnapshot() as T;
  if (command === "get_task_detail") {
    const task = SAMPLE_TASKS.find((item) => item.taskId === args?.taskId) ?? SAMPLE_TASKS[0];
    const turnStatus = task.status === "succeeded" ? "completed"
      : task.status === "failed" ? "failed"
        : task.status === "interrupted" ? "interrupted"
          : task.status === "idle" ? "idle" : "inProgress";
    return { snapshot: task, recentTurn: { turnId: task.turnId ?? "", status: turnStatus, startedAt: task.createdAt, completedAt: task.completedAt, durationSeconds: 1420, planSteps: task.planTotalSteps ? Array.from({ length: task.planTotalSteps }, (_, index) => ({ id: `step-${index + 1}`, title: ["读取任务状态", "同步最新事件", "整理项目汇总", "更新本地快照", "等待下一次校准"][index] ?? "计划步骤", status: index < task.planCompletedSteps ? "completed" : index < task.planCompletedSteps + task.planInProgressSteps ? "inProgress" : "pending" })) : [], error: task.lastError } } as T;
  }
  if (command === "get_settings") return { codexPath: null, codexHome: null, refreshIntervalSeconds: 15, launchAtLogin: true, notificationsEnabled: true, showPromptPreview: false } as T;
  if (command === "update_settings") return args?.settings as T;
  if (command === "get_diagnostics") return { codexPath: sampleSync.codexPath, codexHome: sampleSync.codexHome, codexVersion: "codex-cli 0.147.0", connectionStatus: "online", transport: "stdio", compatibilityMode: false, degradationReason: null } as T;
  return undefined as T;
}

export async function listenEvent<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  if (!isTauri()) return () => undefined;
  return tauriListen<T>(event, (event) => handler(event.payload));
}
