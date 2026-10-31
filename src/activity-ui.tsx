import { useEffect, useRef, useState } from "react";
import {
  AlertCircle,
  CheckCircle2,
  CircleDashed,
  CircleStop,
  Database,
  ExternalLink,
  Layers3,
  LoaderCircle,
} from "lucide-react";
import type {
  ActivitySnapshot,
  ActivitySummary,
  AgentClient,
  DailyUsage,
  ExecutionRecord,
  ExecutionStatus,
  QuotaWindow,
  TokenUsage,
} from "./types";
import { invokeCommand } from "./lib/tauri";
export { executionTitle, executionTitleDetails } from "./lib/display-text";

export const clients: AgentClient[] = ["codex", "claude", "zcode"];
export const clientNames: Record<AgentClient, string> = {
  codex: "Codex",
  claude: "Claude Code",
  zcode: "ZCode",
};

export function OpenExecutionButton({
  execution,
  onError,
  compact = true,
}: {
  execution: ExecutionRecord;
  onError: (error: string | null) => void;
  compact?: boolean;
}) {
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  if (execution.client !== "codex" && execution.client !== "zcode") return null;
  const label =
    execution.client === "zcode"
      ? "在 ZCode 中打开项目"
      : `在 ${clientNames[execution.client]} 中打开`;
  const open = async () => {
    if (pending.current) return;
    pending.current = true;
    setBusy(true);
    onError(null);
    try {
      await invokeCommand<void>("open_execution", { id: execution.id });
    } catch (error) {
      if (mounted.current) onError(message(error));
    } finally {
      pending.current = false;
      if (mounted.current) setBusy(false);
    }
  };
  return (
    <button
      type="button"
      className={
        compact ? "icon-button execution-open-button" : "primary-button"
      }
      aria-label={label}
      aria-busy={busy}
      title={label}
      disabled={busy}
      onClick={() => void open()}
    >
      {busy ? (
        <LoaderCircle size={14} className="spin" />
      ) : (
        <ExternalLink size={14} />
      )}
      {!compact && label}
    </button>
  );
}
export const statusLabels: Record<ExecutionStatus, string> = {
  running: "运行中",
  needs_action: "待处理",
  succeeded: "已完成",
  failed: "失败",
  interrupted: "已中断",
  unknown: "未知",
};
export const statusTones: Record<ExecutionStatus, string> = {
  running: "blue",
  needs_action: "amber",
  succeeded: "green",
  failed: "red",
  interrupted: "slate",
  unknown: "muted",
};
export const quotaLabels: Record<QuotaWindow["state"], string> = {
  available: "已同步",
  unavailable: "未提供",
  unsupported: "不支持",
  stale: "已过期",
  error: "读取失败",
};
export type TokenMetric =
  "total" | "input" | "output" | "cacheRead" | "cacheWrite" | "reasoning";
export type ClientFilter = "all" | AgentClient;
export type UsageRange = 1 | 7 | 14 | 30 | { from: string; through: string };
export const metricLabels: Record<TokenMetric, string> = {
  total: "已记录总量",
  input: "非缓存输入",
  output: "输出",
  cacheRead: "缓存读取",
  cacheWrite: "缓存写入",
  reasoning: "推理细分",
};

export function summaryForClient(
  snapshot: ActivitySnapshot | null,
  client: ClientFilter,
): ActivitySummary | null {
  if (snapshot === null) return null;
  return client === "all" ? snapshot.summary : snapshot.clientSummaries[client];
}

export function sumRecorded(values: Array<number | null>): number | null {
  const recorded = values.filter((value): value is number => value !== null);
  return recorded.length === 0
    ? null
    : recorded.reduce((total, value) => total + value, 0);
}

export function dailyTokenUsage(rows: DailyUsage[]): TokenUsage {
  // DailyUsage is normalized by the backend: cache is separate, reasoning stays inside output.
  return {
    input: sumRecorded(rows.map((row) => row.tokens.input)),
    output: sumRecorded(rows.map((row) => row.tokens.output)),
    cacheRead: sumRecorded(rows.map((row) => row.tokens.cacheRead)),
    cacheWrite: sumRecorded(rows.map((row) => row.tokens.cacheWrite)),
    reasoning: sumRecorded(rows.map((row) => row.tokens.reasoning)),
    total: sumRecorded(rows.map((row) => row.tokens.total)),
    inputIncludesCache: false,
    outputIncludesReasoning: true,
  };
}

export function calendarDates(through: string, count: number): string[] {
  const end = new Date(through);
  end.setHours(12, 0, 0, 0);
  return Array.from({ length: count }, (_, index) => {
    const date = new Date(end);
    date.setDate(end.getDate() - count + index + 1);
    return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
  });
}

export function datesInRange(from: string, through: string): string[] {
  const start = new Date(`${from}T00:00:00Z`);
  const end = new Date(`${through}T00:00:00Z`);
  if (!/^\d{4}-\d{2}-\d{2}$/.test(from) || !/^\d{4}-\d{2}-\d{2}$/.test(through)
    || !Number.isFinite(start.getTime()) || !Number.isFinite(end.getTime())
    || start.toISOString().slice(0, 10) !== from || end.toISOString().slice(0, 10) !== through
    || start > end) return [];
  const count = Math.floor((end.getTime() - start.getTime()) / 86400000) + 1;
  return Array.from({ length: count }, (_, index) =>
    new Date(start.getTime() + index * 86400000).toISOString().slice(0, 10));
}

export function ClientFilters({
  snapshot,
  value,
  onChange,
  compact = false,
}: {
  snapshot: ActivitySnapshot | null;
  value: ClientFilter;
  onChange: (client: ClientFilter) => void;
  compact?: boolean;
}) {
  return (
    <div
      className={`client-filters ${compact ? "compact" : ""}`}
      role="group"
      aria-label="客户端筛选"
    >
      {(["all", ...clients] as ClientFilter[]).map((client) => {
        const summary = summaryForClient(snapshot, client);
        const health =
          client === "all"
            ? undefined
            : snapshot?.sources.find((source) => source.client === client);
        const label = client === "all" ? "全部" : clientNames[client];
        return (
          <button
            key={client}
            className={`client-filter ${client} ${value === client ? "selected" : ""}`}
            aria-label={`筛选${label}`}
            aria-pressed={value === client}
            onClick={() => onChange(client)}
            title={health?.detail}
          >
            <span className="client-filter-main">
              {client === "all" ? (
                <Layers3 size={16} />
              ) : (
                <ClientMark client={client} />
              )}
              <span>{label}</span>
              <strong title="已记录主轮次">
                {summary?.totalTurns.toLocaleString("zh-CN") ?? "—"}
              </strong>
            </span>
            {!compact && (
              <span className="client-filter-meta">
                <span>
                  {summary === null
                    ? "未同步"
                    : `${summary.running} 运行 · ${summary.completedToday} 今日完成`}
                </span>
                {client !== "all" && (
                  <i
                    className={`source-dot ${health?.state ?? "unavailable"}`}
                    aria-label={
                      health === undefined
                        ? "未读取"
                        : health.state === "available"
                          ? "来源可读"
                          : health.state === "error"
                            ? "读取失败"
                            : "来源不可用"
                    }
                  />
                )}
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}

export function message(error: unknown): string {
  return error instanceof Error
    ? error.message
    : typeof error === "string"
      ? error
      : "操作失败";
}
export function relativeTime(value: string | null): string {
  if (value === null) return "未同步";
  const stamp = Date.parse(value);
  if (!Number.isFinite(stamp)) return "时间未知";
  const seconds = Math.max(0, Math.floor((Date.now() - stamp) / 1000));
  if (seconds < 10) return "刚刚";
  if (seconds < 60) return `${seconds} 秒前`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return `${Math.floor(seconds / 86400)} 天前`;
}

export function localTimeZone(): string {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || "系统时区";
}

export function localDateTimeSeconds(value: number | null, suffix = ""): string {
  if (value === null || !Number.isFinite(value)) return "时间未知";
  const formatted = new Intl.DateTimeFormat("zh-CN", {
    timeZone: localTimeZone(),
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(new Date(value * 1000));
  return `${formatted}${suffix} · ${localTimeZone()}`;
}

export function dateTime(value: string | null): string {
  return value === null
    ? "未记录"
    : new Date(value).toLocaleString("zh-CN", { hour12: false });
}
export function formatTokens(value: number | null): string {
  if (value === null) return "未提供";
  if (value < 1000) return value.toLocaleString("zh-CN");
  if (value < 1_000_000)
    return `${(value / 1000).toLocaleString("zh-CN", { maximumFractionDigits: 1 })}k`;
  if (value < 1_000_000_000)
    return `${(value / 1_000_000).toLocaleString("zh-CN", { maximumFractionDigits: 2 })}M`;
  return `${(value / 1_000_000_000).toLocaleString("zh-CN", { maximumFractionDigits: 2 })}B`;
}
export function formatDuration(ms: number | null): string {
  if (ms === null) return "未提供";
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} 分 ${seconds % 60} 秒`;
  return `${Math.floor(minutes / 60)} 时 ${minutes % 60} 分`;
}
export function elapsed(
  execution: ExecutionRecord,
  now: number,
): number | null {
  if (
    execution.live &&
    execution.startedAt !== null &&
    execution.completedAt === null
  )
    return Math.max(0, now - Date.parse(execution.startedAt));
  return execution.durationMs;
}
export function projectName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}
export function StatusIcon({
  status,
  size = 15,
}: {
  status: ExecutionStatus;
  size?: number;
}) {
  if (status === "running")
    return <LoaderCircle className="spin" size={size} />;
  if (status === "needs_action") return <AlertCircle size={size} />;
  if (status === "succeeded") return <CheckCircle2 size={size} />;
  if (status === "failed") return <AlertCircle size={size} />;
  if (status === "interrupted") return <CircleStop size={size} />;
  return <CircleDashed size={size} />;
}
export function ClientMark({ client }: { client: AgentClient }) {
  return (
    <span className={`client-mark ${client}`}>
      {client === "codex" ? "C" : client === "claude" ? "A" : "Z"}
    </span>
  );
}
export function Empty({
  title,
  detail,
  loading = false,
}: {
  title: string;
  detail?: string;
  loading?: boolean;
}) {
  return (
    <div className="empty-state activity-empty">
      <div className="empty-icon">
        {loading ? (
          <LoaderCircle size={22} className="spin" />
        ) : (
          <Database size={22} />
        )}
      </div>
      <h3>{title}</h3>
      {detail !== undefined && <p>{detail}</p>}
    </div>
  );
}
export function Toggle({
  label,
  checked,
  onChange,
  hint,
}: {
  label: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  hint?: string;
}) {
  return (
    <label className="toggle-row">
      <span>
        <strong>{label}</strong>
        {hint !== undefined && <small>{hint}</small>}
      </span>
      <input
        type="checkbox"
        checked={checked}
        onChange={(event) => onChange(event.target.checked)}
      />
      <span className="toggle-ui" />
    </label>
  );
}
export function TokenBreakdown({
  usage,
  activeMetric,
}: {
  usage: TokenUsage;
  activeMetric?: TokenMetric;
}) {
  const fields: Array<
    [
      keyof Pick<
        TokenUsage,
        "input" | "output" | "cacheRead" | "cacheWrite" | "reasoning" | "total"
      >,
      string,
    ]
  > = [
    ["input", usage.inputIncludesCache ? "输入（含缓存）" : "非缓存输入"],
    ["output", usage.outputIncludesReasoning ? "输出（含推理）" : "输出"],
    ["cacheRead", usage.inputIncludesCache ? "其中缓存读取" : "缓存读取"],
    ["cacheWrite", "缓存写入"],
    ["reasoning", usage.outputIncludesReasoning ? "其中推理输出" : "推理输出"],
    ["total", "已记录总量"],
  ];
  return (
    <dl className="token-breakdown">
      {fields.map(([key, label]) => (
        <div key={key} className={activeMetric === key ? "active" : ""}>
          <dt>{label}</dt>
          <dd
            title={
              usage[key] === null
                ? undefined
                : usage[key].toLocaleString("zh-CN")
            }
          >
            {formatTokens(usage[key])}
          </dd>
        </div>
      ))}
    </dl>
  );
}
