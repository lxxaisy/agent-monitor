import { Fragment, useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { emitTo } from "@tauri-apps/api/event";
import {
  Activity,
  AlertCircle,
  ArrowUpRight,
  Bell,
  ChartNoAxesCombined,
  CheckCircle2,
  ChevronRight,
  Clock3,
  GripHorizontal,
  History,
  LayoutDashboard,
  LoaderCircle,
  MoveDiagonal2,
  PanelTop,
  Radar,
  Radio,
  RefreshCw,
  Settings as SettingsIcon,
  ShieldCheck,
  Usb,
  Wallet,
  X,
} from "lucide-react";
import {
  currentWindowLabel,
  hideAppWindow,
  invokeCommand,
  isLocalPreview,
  isNativeApp,
  listenEvent,
  showAppWindow,
  startCurrentWindowDragging,
  startCurrentWindowResize,
} from "./lib/tauri";
import {
  ExecutionDetail,
  HistoryView,
  Overview,
  UsageView,
} from "./ActivityViews";
import {
  AlertsView,
  PassportView,
  QuotasView,
  SettingsView,
  ResetNewsSection,
  ResetNewsSpotlight,
} from "./ConnectionViews";
import {
  ClientMark,
  ClientFilters,
  OpenExecutionButton,
  StatusIcon,
  clients,
  clientNames,
  dateTime,
  elapsed,
  executionTitle,
  formatDuration,
  formatTokens,
  message,
  relativeTime,
  statusTones,
  statusLabels,
  summaryForClient,
  type ClientFilter,
  type TokenMetric,
  type UsageRange,
} from "./activity-ui";
import type {
  ActivitySnapshot,
  AlertRecord,
  Diagnostics,
  DesktopNotice,
  ExecutionRecord,
  ExecutionStatus,
  PassportState,
  NewsSnapshot,
  QuotaSnapshot,
  Settings,
  ThemePreference,
} from "./types";
import "./styles.css";

type View =
  | "news"
  | "overview"
  | "history"
  | "usage"
  | "quotas"
  | "passport"
  | "alerts"
  | "settings";
const viewLabels: Record<View, string> = {
  news: "Tibo 动态",
  overview: "工作总览",
  history: "执行历史",
  usage: "Token 用量",
  quotas: "账户额度",
  passport: "AI Passport",
  alerts: "提醒",
  settings: "设置与诊断",
};

function applyTheme(theme: ThemePreference) {
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme =
    theme === "system" ? "light dark" : theme;
}

function useVisibleClock(
  onResume: () => void,
  onError: (error: unknown) => void,
) {
  const [visible, setVisible] = useState(
    () => !isNativeApp() && document.visibilityState === "visible",
  );
  const [now, setNow] = useState(Date.now());
  const visibleRef = useRef(visible);
  const callbacks = useRef({ onResume, onError });
  callbacks.current = { onResume, onError };
  useEffect(() => {
    let disposed = false;
    let checkId = 0;
    let focusRequested = false;
    const cleanups: Array<() => void> = [];
    const check = async (reason: "initial" | "visibility" | "focus") => {
      if (reason === "focus") focusRequested = true;
      const id = ++checkId;
      try {
        const next = isNativeApp()
          ? await getCurrentWindow().isVisible()
          : document.visibilityState === "visible";
        if (disposed || id !== checkId) return;
        const wasVisible = visibleRef.current;
        visibleRef.current = next;
        setVisible(next);
        const resumeFocus = focusRequested;
        focusRequested = false;
        if (next && (resumeFocus || (reason === "visibility" && !wasVisible))) {
          setNow(Date.now());
          callbacks.current.onResume();
        }
      } catch (error) {
        if (!disposed) callbacks.current.onError(error);
      }
    };
    const keep = (pending: Promise<() => void>) => {
      void pending
        .then((cleanup) => {
          if (disposed) cleanup();
          else cleanups.push(cleanup);
        })
        .catch((error) => {
          if (!disposed) callbacks.current.onError(error);
        });
    };
    const visibilityChanged = () => void check("visibility");
    document.addEventListener("visibilitychange", visibilityChanged);
    if (isNativeApp()) {
      const current = getCurrentWindow();
      keep(
        current.onFocusChanged(
          ({ payload }) => void check(payload ? "focus" : "visibility"),
        ),
      );
      keep(
        listenEvent<{ label: string; visible: boolean }>(
          "monitor://window-visibility-changed",
          (event) => {
            if (event.label === current.label) void check("visibility");
          },
        ),
      );
    } else {
      const focused = () => void check("focus");
      window.addEventListener("focus", focused);
      cleanups.push(() => window.removeEventListener("focus", focused));
    }
    void check("initial");
    return () => {
      disposed = true;
      document.removeEventListener("visibilitychange", visibilityChanged);
      cleanups.forEach((cleanup) => cleanup());
    };
  }, []);
  useEffect(() => {
    if (!visible) return;
    setNow(Date.now());
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(tick);
  }, [visible]);
  return now;
}

function NotificationWindow() {
  const [notices, setNotices] = useState<DesktopNotice[]>([]);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<{
    id: string | null;
    detail: string;
  } | null>(null);
  const now = useVisibleClock(
    () => {
      void invokeCommand<DesktopNotice[]>("get_desktop_notices")
        .then(setNotices)
        .catch((caught) => setError({ id: null, detail: message(caught) }));
    },
    (caught) => setError({ id: null, detail: message(caught) }),
  );
  useEffect(() => {
    void invokeCommand<Settings>("get_settings")
      .then((settings) => applyTheme(settings.theme))
      .catch(() => applyTheme("system"));
  }, []);
  useEffect(() => {
    document.body.classList.add("notices-window");
    document.documentElement.classList.add("notices-window");
    let disposed = false;
    let unlisten: (() => void) | null = null;
    let unlistenSettings: (() => void) | null = null;
    void listenEvent<DesktopNotice[]>(
      "monitor://desktop-notices-updated",
      (next) => {
        if (!disposed) setNotices(next);
      },
    )
      .then((cleanup) => {
        if (disposed) cleanup();
        else unlisten = cleanup;
      })
      .catch((caught) => {
        if (!disposed) setError({ id: null, detail: message(caught) });
      });
    void listenEvent<Settings>("monitor://settings-updated", (settings) => {
      if (!disposed) applyTheme(settings.theme);
    })
      .then((cleanup) => {
        if (disposed) cleanup();
        else unlistenSettings = cleanup;
      })
      .catch((caught) => {
        if (!disposed) setError({ id: null, detail: message(caught) });
      });
    void invokeCommand<DesktopNotice[]>("get_desktop_notices")
      .then((next) => {
        if (!disposed) setNotices(next);
      })
      .catch((caught) => {
        if (!disposed) setError({ id: null, detail: message(caught) });
      });
    return () => {
      disposed = true;
      unlisten?.();
      unlistenSettings?.();
      document.body.classList.remove("notices-window");
      document.documentElement.classList.remove("notices-window");
    };
  }, []);
  const act = async (
    command: "dismiss_desktop_notice" | "open_desktop_notice",
    id: string,
  ) => {
    setPending(id);
    setError(null);
    try {
      setNotices(await invokeCommand<DesktopNotice[]>(command, { id }));
    } catch (caught) {
      setError({ id, detail: message(caught) });
    } finally {
      setPending(null);
    }
  };
  return (
    <main className="notices-shell" aria-label="桌面提醒">
      <div className="notices-list">
        {notices.length === 0 ? (
          <div className="notices-empty">
            {error === null ? <Bell size={17} /> : <AlertCircle size={17} />}
            <span>{error?.detail ?? "暂无桌面提醒"}</span>
          </div>
        ) : (
          notices.map((notice) => {
            const status =
              notice.kind === "completed"
                ? "succeeded"
                : notice.kind === "failed"
                  ? "failed"
                  : "needs_action";
            const remaining = Math.max(
              0,
              Math.ceil((Date.parse(notice.expiresAt) - now) / 1000),
            );
            return (
              <article
                className={`desktop-notice ${statusTones[status]}`}
                key={notice.id}
                data-notice-id={notice.id}
              >
                <header className="notice-header">
                  <span className="notice-kind">
                    <StatusIcon status={status} size={15} />
                    {notice.kind === "reset_news" ? "重置消息" : statusLabels[status]}
                  </span>
                  <time
                    dateTime={notice.createdAt}
                    title={dateTime(notice.createdAt)}
                  >
                    {relativeTime(notice.createdAt)}
                  </time>
                  <button
                    className="notice-close"
                    aria-label="关闭通知"
                    title="关闭通知"
                    disabled={pending !== null}
                    onClick={() =>
                      void act("dismiss_desktop_notice", notice.id)
                    }
                  >
                    <X size={15} />
                  </button>
                </header>
                <button
                  className="notice-open"
                  aria-label={notice.kind === "reset_news" ? "查看重置公告" : "查看任务详情"}
                  title={executionTitle(notice.title)}
                  disabled={pending !== null}
                  onClick={() => void act("open_desktop_notice", notice.id)}
                >
                  <strong>{executionTitle(notice.title)}</strong>
                  <ArrowUpRight size={14} />
                </button>
                <div
                  className={`notice-footer ${error?.id === notice.id ? "error" : ""}`}
                >
                  {error?.id === notice.id ? (
                    <span title={error.detail}>{error.detail}</span>
                  ) : (
                    <>
                      <Clock3 size={10} />
                      <span>
                        {remaining === 0
                          ? "即将关闭"
                          : `${Math.floor(remaining / 60)}:${String(remaining % 60).padStart(2, "0")} 后自动关闭`}
                      </span>
                    </>
                  )}
                </div>
              </article>
            );
          })
        )}
      </div>
    </main>
  );
}

function QuickPanel({
  snapshot,
  news,
  alerts,
  loading,
  error,
  now,
  client,
  onClientChange,
  onSelect,
  onSync,
  onConsole,
  onHide,
  onOpenNews,
}: {
  snapshot: ActivitySnapshot | null;
  news: NewsSnapshot | null;
  alerts: AlertRecord[];
  loading: boolean;
  error: string | null;
  now: number;
  client: ClientFilter;
  onClientChange: (client: ClientFilter) => void;
  onSelect: (execution: ExecutionRecord) => void;
  onSync: () => void;
  onConsole: () => void;
  onHide: () => void;
  onOpenNews: () => void;
}) {
  const [openError, setOpenError] = useState<{
    id: string;
    detail: string;
  } | null>(null);
  const summary = summaryForClient(snapshot, client);
  const roots =
    snapshot?.executions.filter(
      (item) => !item.isChild && (client === "all" || item.client === client),
    ) ?? [];
  const relevant = roots.filter(
    (item) =>
      (item.live &&
        (item.status === "running" || item.status === "needs_action")) ||
      (item.status === "succeeded" &&
        item.completedAt !== null &&
        new Date(item.completedAt).toDateString() ===
          new Date(now).toDateString()),
  );
  const priority = (item: ExecutionRecord) =>
    item.status === "needs_action" ? 0 : item.status === "running" ? 1 : 2;
  const recent = relevant
    .sort(
      (a, b) =>
        priority(a) - priority(b) ||
        Date.parse(b.observedAt) - Date.parse(a.observedAt),
    )
    .slice(0, 5);
  const headline =
    summary === null
      ? loading
        ? "正在读取本机任务"
        : "等待本机数据"
      : summary.needsAction > 0
        ? `${summary.needsAction} 个任务需要处理`
        : summary.running > 0
          ? `${summary.running} 个任务正在执行`
          : summary.completedToday > 0
            ? `${summary.completedToday} 个任务今日完成`
            : "当前没有执行中的任务";
  return (
    <div className="quick-shell">
      <button
        className="quick-drag-handle"
        onPointerDown={(event) => {
          if (event.button === 0) {
            event.preventDefault();
            void startCurrentWindowDragging();
          }
        }}
        title="拖动浮窗"
        aria-label="拖动浮窗"
      >
        <GripHorizontal size={18} />
      </button>
      <header className="quick-header">
        <div
          className="quick-brand"
          onPointerDown={(event) => {
            if (event.button === 0) {
              event.preventDefault();
              void startCurrentWindowDragging();
            }
          }}
        >
          <span className="quick-brand-mark">
            <Radar size={16} />
          </span>
          <div>
            <span className="quick-eyebrow">AGENT MONITOR</span>
            <strong>任务监控</strong>
          </div>
        </div>
        <div className="quick-actions">
          <button
            className="quick-icon-button"
            onClick={onSync}
            title="立即同步"
            aria-label="立即同步"
          >
            <RefreshCw size={15} className={loading ? "spin" : ""} />
          </button>
          <button
            className="quick-icon-button"
            onClick={onHide}
            title="隐藏浮窗"
            aria-label="隐藏浮窗"
          >
            <X size={16} />
          </button>
        </div>
      </header>
      <section
        className={`quick-hero ${summary !== null && summary.needsAction > 0 ? "attention" : summary !== null && summary.running > 0 ? "active" : "quiet"}`}
      >
        <div className="quick-hero-copy">
          <span className="quick-connection">
            <span
              className={`quick-connection-dot ${snapshot === null ? "offline" : "online"}`}
            />
            {client === "all" ? "全部客户端" : clientNames[client]}
          </span>
          <strong>{headline}</strong>
          <small>
            {snapshot === null
              ? (error ?? "等待采集器")
              : `更新于 ${relativeTime(snapshot.generatedAt)}`}
          </small>
        </div>
      </section>
      <ResetNewsSpotlight news={news} onOpen={onOpenNews}/>
      <ClientFilters
        snapshot={snapshot}
        value={client}
        onChange={onClientChange}
        compact
      />
      <div className="quick-stats">
        {(
          [
            ["运行中", summary?.running, "blue"],
            ["待处理", summary?.needsAction, "amber"],
            ["今日完成", summary?.completedToday, "green"],
          ] as const
        ).map(([label, value, tone]) => (
          <div key={label}>
            <strong className={tone}>{value ?? "—"}</strong>
            <span>{label}</span>
          </div>
        ))}
      </div>
      <div className="quick-section-heading">
        <div>
          <span className="quick-section-kicker">
            <Activity size={12} />
            主任务
          </span>
          <strong>{recent.length} 个关注任务</strong>
        </div>
        <button className="quick-open-button" onClick={onConsole}>
          查看控制台
          <ArrowUpRight size={14} />
        </button>
      </div>
      <div className="quick-task-list">
        {recent.length === 0 ? (
          <div className="quick-empty">
            {loading ? (
              <LoaderCircle size={16} className="spin" />
            ) : (
              <CheckCircle2 size={16} />
            )}
            <span>{snapshot === null ? "等待连接" : "暂无关注任务"}</span>
          </div>
        ) : (
          recent.map((item) => (
            <div className="quick-task-row" key={item.id}>
              <button
                className={`quick-task ${statusTones[item.status]}`}
                data-execution-id={item.id}
                title="查看执行详情"
                onClick={() => onSelect(item)}
              >
                <span className="quick-task-icon">
                  <StatusIcon status={item.status} />
                </span>
                <span className="quick-task-copy">
                  <strong>{executionTitle(item.title)}</strong>
                  <small>
                    {clientNames[item.client]}
                    <span>·</span>第 {item.turnIndex} 轮
                  </small>
                  <span className="quick-task-duration">
                    {formatDuration(elapsed(item, now))}
                    <span>{formatTokens(item.tokens.total)} Token</span>
                  </span>
                </span>
                <ChevronRight size={14} className="quick-task-arrow" />
              </button>
              <span className="execution-source-action">
                <OpenExecutionButton
                  execution={item}
                  onError={(detail) =>
                    setOpenError(
                      detail === null ? null : { id: item.id, detail },
                    )
                  }
                />
              </span>
              {openError?.id === item.id && (
                <div className="execution-open-error" role="alert">
                  {openError.detail}
                </div>
              )}
            </div>
          ))
        )}
      </div>
      <footer className="quick-footer">
        <span>
          <ShieldCheck size={12} />
          全局未读 {alerts.filter((alert) => alert.unread).length}
        </span>
        <button onClick={onConsole}>
          控制台
          <ArrowUpRight size={12} />
        </button>
      </footer>
      <button
        className="quick-resize-grip"
        onPointerDown={(event) => {
          if (event.button === 0) {
            event.preventDefault();
            void startCurrentWindowResize("SouthEast");
          }
        }}
        title="调整浮窗大小"
        aria-label="调整浮窗大小"
      >
        <MoveDiagonal2 size={13} />
      </button>
    </div>
  );
}

export default function App() {
  const windowLabel = currentWindowLabel();
  return windowLabel === "notices" ? (
    <NotificationWindow />
  ) : (
    <MonitorApp windowLabel={windowLabel} />
  );
}

function MonitorApp({ windowLabel }: { windowLabel: "main" | "quick" }) {
  const [view, setView] = useState<View>(() => new URLSearchParams(window.location.search).get("view") === "news" ? "news" : "overview");
  const [client, setClient] = useState<ClientFilter>("all");
  const [historyStatus, setHistoryStatus] = useState<"all" | ExecutionStatus>(
    "all",
  );
  const [usageRange, setUsageRange] = useState<UsageRange>(14);
  const [usageMetric, setUsageMetric] = useState<TokenMetric>("total");
  const [activity, setActivity] = useState<ActivitySnapshot | null>(null);
  const [quota, setQuota] = useState<QuotaSnapshot | null>(null);
  const [news, setNews] = useState<NewsSnapshot | null>(null);
  const [newsError, setNewsError] = useState<string | null>(null);
  const [passport, setPassport] = useState<PassportState | null>(null);
  const [alerts, setAlerts] = useState<AlertRecord[]>([]);
  const [settings, setSettings] = useState<Settings>({
    codexPath: null,
    codexHome: null,
    refreshIntervalSeconds: 15,
    historyRetentionDays: 30,
    launchAtLogin: true,
    notificationsEnabled: true,
    notificationDurationSeconds: 300,
    showPromptPreview: false,
    theme: "system",
  });
  const [diagnostics, setDiagnostics] = useState<Diagnostics | null>(null);
  const [selected, setSelected] = useState<ExecutionRecord | null>(null);
  const [loading, setLoading] = useState(true);
  const [quotaLoading, setQuotaLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [activityError, setActivityError] = useState<string | null>(null);
  const [syncError, setSyncError] = useState<string | null>(null);
  const refreshActivity = useCallback(async () => {
    try {
      const next = await invokeCommand<ActivitySnapshot>(
        "get_activity_snapshot",
      );
      setActivity(next);
      setActivityError(null);
      setSelected((current) =>
        current === null
          ? null
          : (next.executions.find((item) => item.id === current.id) ?? current),
      );
    } catch (error) {
      setActivityError(message(error));
    } finally {
      setLoading(false);
    }
  }, []);
  const refreshQuota = useCallback(async (force = false) => {
    setQuotaLoading(true);
    try {
      setQuota(
        await invokeCommand<QuotaSnapshot>(
          force ? "refresh_quotas" : "get_quota_snapshot",
        ),
      );
    } catch (error) {
      setError(message(error));
    } finally {
      setQuotaLoading(false);
    }
  }, []);
  const refreshAuxiliary = useCallback(async () => {
    const results = await Promise.allSettled([
      invokeCommand<PassportState>("get_passport_state"),
      invokeCommand<AlertRecord[]>("get_alerts"),
      invokeCommand<NewsSnapshot>("get_reset_news"),
    ]);
    if (results[0].status === "fulfilled") setPassport(results[0].value);
    if (results[1].status === "fulfilled") setAlerts(results[1].value);
    if (results[2].status === "fulfilled") { setNews(results[2].value); setNewsError(null); }
    else setNewsError(message(results[2].reason));
  }, []);
  const refreshDiagnostics = useCallback(async () => {
    try {
      setDiagnostics(await invokeCommand<Diagnostics>("get_diagnostics"));
    } catch {}
  }, []);
  const resumePending = useRef<Promise<unknown> | null>(null);
  const resume = useCallback(() => {
    if (resumePending.current !== null) return;
    const pending = Promise.all([
      refreshActivity(),
      refreshQuota(),
      refreshAuxiliary(),
      refreshDiagnostics(),
    ]);
    resumePending.current = pending;
    void pending.finally(() => {
      if (resumePending.current === pending) resumePending.current = null;
    });
  }, [refreshActivity, refreshQuota, refreshAuxiliary, refreshDiagnostics]);
  const now = useVisibleClock(resume, (error) => setError(message(error)));
  const sync = useCallback(async () => {
    setLoading(true);
    setError(null);
    setSyncError(null);
    try {
      await invokeCommand("sync_now");
      await Promise.all([
        refreshActivity(),
        refreshQuota(true),
        refreshAuxiliary(),
        refreshDiagnostics(),
      ]);
    } catch (error) {
      setSyncError(message(error));
    } finally {
      setLoading(false);
    }
  }, [refreshActivity, refreshQuota, refreshAuxiliary, refreshDiagnostics]);
  useEffect(() => {
    applyTheme(settings.theme);
  }, [settings.theme]);
  useEffect(() => {
    document.documentElement.classList.toggle("native-glass", isNativeApp());
    document.body.classList.toggle("quick-window", windowLabel === "quick");
    document.documentElement.classList.toggle(
      "quick-window",
      windowLabel === "quick",
    );
    return () => {
      document.body.classList.remove("quick-window");
      document.documentElement.classList.remove("quick-window");
      document.documentElement.classList.remove("native-glass");
      applyTheme("system");
    };
  }, [windowLabel]);
  useEffect(() => {
    let disposed = false;
    const cleanups: Array<() => void> = [];
    const register = <T,>(name: string, handler: (value: T) => void) => {
      void listenEvent<T>(name, (value) => {
        if (!disposed) handler(value);
      })
        .then((cleanup) => {
          if (disposed) cleanup();
          else cleanups.push(cleanup);
        })
        .catch((error) => {
          if (!disposed) setError(message(error));
        });
    };
    void refreshActivity();
    void refreshQuota();
    void refreshAuxiliary();
    void refreshDiagnostics();
    void invokeCommand<Settings>("get_settings")
      .then((value) => {
        if (!disposed) setSettings(value);
      })
      .catch(() => {});
    register<ActivitySnapshot>("monitor://activity-updated", (next) => {
      setActivity(next);
      setActivityError(null);
      setSyncError(null);
      setSelected((current) =>
        current === null
          ? null
          : (next.executions.find((item) => item.id === current.id) ?? current),
      );
    });
    register<QuotaSnapshot>("monitor://quota-updated", setQuota);
    register<NewsSnapshot>("monitor://reset-news-updated", (next) => {setNews(next);setNewsError(null);});
    register<PassportState>("monitor://passport-updated", setPassport);
    register<AlertRecord[]>("monitor://alerts-updated", setAlerts);
    register<Settings>("monitor://settings-updated", setSettings);
    register<unknown>(
      "monitor://connection-changed",
      () => void refreshDiagnostics(),
    );
    register<{ taskId: string }>("monitor://open-task-detail", ({ taskId }) => {
      if (taskId.startsWith("reset:")) { setView("news"); setSelected(null); return; }
      void invokeCommand<ExecutionRecord | null>("get_execution_detail", {
        id: taskId,
      })
        .then((record) => {
          if (disposed) return;
          if (record !== null) setSelected(record);
          else setError("该执行记录已不可用。");
        })
        .catch((error) => {
          if (!disposed) setError(message(error));
        });
    });
    return () => {
      disposed = true;
      cleanups.forEach((cleanup) => cleanup());
    };
  }, [refreshActivity, refreshQuota, refreshAuxiliary, refreshDiagnostics]);
  useEffect(() => {
    if (isNativeApp()) return;
    const timer = window.setInterval(
      () => void refreshActivity(),
      Math.max(5, settings.refreshIntervalSeconds) * 1000,
    );
    const aux = window.setInterval(() => {
      void refreshAuxiliary();
      void refreshDiagnostics();
    }, 5000);
    return () => {
      window.clearInterval(timer);
      window.clearInterval(aux);
    };
  }, [
    refreshActivity,
    refreshAuxiliary,
    refreshDiagnostics,
    settings.refreshIntervalSeconds,
  ]);
  const navigate = (next: View) => {
    setView(next);
    setSelected(null);
    window.scrollTo({ top: 0, behavior: "auto" });
  };
  const selectAlert = (id: string) => {
    if (id.startsWith("reset:")) { navigate("news"); return; }
    const execution = activity?.executions.find((item) => item.id === id);
    if (execution !== undefined) setSelected(execution);
    else
      void invokeCommand<ExecutionRecord | null>("get_execution_detail", { id })
        .then((record) => {
          if (record !== null) setSelected(record);
          else setError("该执行记录已不可用。");
        })
        .catch((error) => setError(message(error)));
  };
  const ack = async (id: string) => {
    await invokeCommand("acknowledge_alert", { id });
    setAlerts(await invokeCommand<AlertRecord[]>("get_alerts"));
  };
  const saveSettings = async (next: Settings) => {
    setSettings(
      await invokeCommand<Settings>("update_settings", { settings: next }),
    );
    await refreshAuxiliary();
  };
  const openConsole = () =>
    void showAppWindow("main")
      .catch((error) => setError(message(error)));
  const openNews = () => {
    if (!isNativeApp()) {
      const url = new URL(window.location.href);
      url.searchParams.set("window", "main");
      url.searchParams.set("view", "news");
      window.location.assign(url);
      return;
    }
    void emitTo("main", "monitor://open-task-detail", {taskId: "reset:latest"})
      .then(openConsole).catch(caught => setError(message(caught)));
  };
  const unread = alerts.filter((alert) => alert.unread).length;
  const activityScope =
    view === "overview" || view === "history" || view === "usage";
  const navigation: Array<[View, typeof Activity]> = [
    ["overview", LayoutDashboard],
    ["history", History],
    ["usage", ChartNoAxesCombined],
    ["news", Radio],
    ["quotas", Wallet],
    ["alerts", Bell],
    ["passport", Usb],
    ["settings", SettingsIcon],
  ];
  if (windowLabel === "quick")
    return (
      <>
        <QuickPanel
          snapshot={activity}
          news={news}
          alerts={alerts}
          loading={loading}
          error={activityError}
          now={now}
          client={client}
          onClientChange={setClient}
          onSelect={setSelected}
          onSync={() => void sync()}
          onConsole={openConsole}
          onOpenNews={openNews}
          onHide={() => void hideAppWindow("quick")}
        />
        {selected !== null && (
          <ExecutionDetail
            execution={selected}
            revision={activity?.generatedAt ?? null}
            now={now}
            onSelect={setSelected}
            onClose={() => setSelected(null)}
          />
        )}
      </>
    );
  return (
    <div className="app-shell" data-view={view}>
      <aside className="side-rail">
        <div className="brand">
          <span className="brand-mark">
            <Radar size={17} />
          </span>
          <span className="brand-copy">
            <strong>Agent Monitor</strong>
            <small>本机工作空间</small>
          </span>
        </div>
        <nav className="nav-list">
          <span className="nav-label">工作空间</span>
          {navigation.map(([item, Icon]) => (
            <Fragment key={item}>
            {item === "passport" && <span className="nav-label nav-divider">设备与偏好</span>}
            <button
              className={`nav-item ${view === item ? "active" : ""}`}
              onClick={() => navigate(item)}
              title={viewLabels[item]}
              aria-current={view === item ? "page" : undefined}
            >
              <Icon size={17} />
              <span className="nav-text">{viewLabels[item]}</span>
              {item === "alerts" && unread > 0 && <span className="nav-unread">{unread > 99 ? "99+" : unread}</span>}
              {item === "news" && news?.posts[0]?.state === "announced" && <span className="nav-news-dot" aria-label="公告待确认"/>}
            </button>
            </Fragment>
          ))}
        </nav>
        <div className="rail-bottom">
          <div className="provider-label">本机 Agent</div>
          {clients.map((item) => {
            const source = activity?.sources.find(
              (source) => source.client === item,
            );
            return (
              <button
                className={`provider-row provider-button ${activityScope && client === item ? "selected" : ""}`}
                key={item}
                onClick={() => {
                  setClient(item);
                  if (!activityScope) navigate("overview");
                }}
                title={`筛选${clientNames[item]}`}
                aria-label={`侧栏筛选${clientNames[item]}`}
              >
                <span className="provider-icon">
                  <ClientMark client={item} />
                </span>
                <span>{clientNames[item]}</span>
                <span
                  className={`connection-dot ${source?.state === "available" ? "online" : source?.state === "error" ? "degraded" : "offline"}`}
                  title={source?.detail}
                />
              </button>
            );
          })}
          <div className="privacy-note">
            <ShieldCheck size={14} />
            <span>本地只读模式</span>
          </div>
        </div>
      </aside>
      <main className="main-content">
        <header className="topbar">
          <div className="topbar-title">
            <h1>{viewLabels[view]}</h1>
            {activityScope && (
              <span className="topbar-context">
                {client === "all" ? "全部客户端" : clientNames[client]}
              </span>
            )}
            {view === "quotas" && (
              <span className="topbar-context">服务账户</span>
            )}
          </div>
          <div className="topbar-actions">
            <span
              className={`sync-status ${activity === null ? "offline" : "online"}`}
            >
              <span className="sync-dot" />
              {activity === null
                ? "未同步"
                : relativeTime(activity.generatedAt)}
            </span>
            <button
              className={`icon-button alerts-button ${view === "alerts" ? "active" : ""}`}
              onClick={() => navigate("alerts")}
              title={`提醒${unread > 0 ? ` · ${unread} 条未读` : ""}`}
              aria-label="提醒"
            >
              <Bell size={17} />
              {unread > 0 && <span>{unread > 99 ? "99+" : unread}</span>}
            </button>
            <button
              className="icon-button"
              onClick={() =>
                void showAppWindow("quick")
                  .catch((error) => setError(message(error)))
              }
              aria-label="打开浮窗"
              title="打开浮窗"
            >
              <PanelTop size={17} />
            </button>
            <button
              className="icon-button"
              onClick={() => void sync()}
              disabled={loading}
              aria-label="立即同步"
              title="立即同步"
            >
              <RefreshCw size={17} className={loading ? "spin" : ""} />
            </button>
          </div>
        </header>
        {syncError !== null && (
          <div className="inline-alert sync-failure" role="alert">
            <AlertCircle size={15} />
            <div><strong>本次同步未完成</strong><span>{syncError}</span><small>后台会继续尝试，也可以立即重试。</small></div>
            <button className="secondary-button" disabled={loading} onClick={() => void sync()}><RefreshCw size={14} />重试同步</button>
          </div>
        )}
        {error !== null && (
          <div className="inline-alert">
            <AlertCircle size={15} />
            <span>{error}</span>
            <button onClick={() => setError(null)} aria-label="关闭提示">
              <X size={14} />
            </button>
          </div>
        )}
        {activityError !== null && activity !== null && (
          <div className="offline-alert">
            <AlertCircle size={15} />
            <span>采集暂时不可用，显示最近快照：{activityError}</span>
          </div>
        )}
        {!isNativeApp() && (
          <div className="preview-banner">
            <InfoIcon />
            <span>
              {isLocalPreview()
                ? "本机数据预览"
                : "浏览器尚未连接本机采集器，请使用桌面应用。"}
            </span>
          </div>
        )}
        {activityScope && (
          <ClientFilters
            snapshot={activity}
            value={client}
            onChange={setClient}
          />
        )}
        {view === "overview" && <ResetNewsSpotlight news={news} onOpen={() => navigate("news")}/>}
        {view === "overview" ? (
          <Overview
            snapshot={activity}
            loading={loading}
            error={activityError}
            now={now}
            client={client}
            onSelect={setSelected}
            onHistory={() => navigate("history")}
            selectedId={selected?.id ?? null}
          />
        ) : view === "history" ? (
          <HistoryView
            revision={activity?.generatedAt ?? null}
            now={now}
            client={client}
            status={historyStatus}
            onStatusChange={setHistoryStatus}
            onSelect={setSelected}
            selectedId={selected?.id ?? null}
          />
        ) : view === "usage" ? (
          <UsageView
            snapshot={activity}
            now={now}
            client={client}
            onClientChange={setClient}
            range={usageRange}
            onRangeChange={setUsageRange}
            metric={usageMetric}
            onMetricChange={setUsageMetric}
          />
        ) : view === "quotas" ? (
          <QuotasView
            snapshot={quota}
            loading={quotaLoading}
            onRefresh={() => void refreshQuota(true)}
          />
        ) : view === "news" ? (
          <ResetNewsSection news={news} onNews={next => {setNews(next);setNewsError(null);}} loadError={newsError}/>
        ) : view === "passport" ? (
          <PassportView
            state={passport}
            onState={setPassport}
            onError={setError}
          />
        ) : view === "alerts" ? (
          <AlertsView alerts={alerts} onAck={ack} onSelect={selectAlert} />
        ) : (
          <SettingsView
            settings={settings}
            diagnostics={diagnostics}
            onSave={saveSettings}
          />
        )}
      </main>
      {selected !== null && (
        <ExecutionDetail
          execution={selected}
          revision={activity?.generatedAt ?? null}
          now={now}
          onSelect={setSelected}
          onClose={() => setSelected(null)}
        />
      )}
    </div>
  );
}

function InfoIcon() {
  return <ShieldCheck size={15} />;
}
