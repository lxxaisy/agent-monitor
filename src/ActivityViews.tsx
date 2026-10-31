import { useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  AlertCircle,
  ArrowUpRight,
  ChartNoAxesCombined,
  CalendarDays,
  Check,
  CheckCircle2,
  CircleX,
  ChevronLeft,
  ChevronRight,
  ChevronDown,
  Clock3,
  Clipboard,
  FolderOpen,
  GitBranch,
  PanelRightClose,
  Search,
  ShieldCheck,
  X,
} from "lucide-react";
import { invokeCommand } from "./lib/tauri";
import {
  ClientMark,
  Empty,
  OpenExecutionButton,
  StatusIcon,
  TokenBreakdown,
  calendarDates,
  clients,
  clientNames,
  dateTime,
  dailyTokenUsage,
  datesInRange,
  elapsed,
  executionTitle,
  executionTitleDetails,
  formatDuration,
  formatTokens,
  message,
  metricLabels,
  projectName,
  statusLabels,
  statusTones,
  summaryForClient,
  sumRecorded,
  type ClientFilter,
  type TokenMetric,
  type UsageRange,
} from "./activity-ui";
import type {
  ActivityPage,
  ActivityQuery,
  ActivitySnapshot,
  DailyUsage,
  ExecutionRecord,
  ExecutionStatus,
  SessionPage,
  SessionSummary,
} from "./types";

export function ExecutionRow({
  execution,
  now,
  onSelect,
  nested = false,
  selectedId = null,
  roundOnly = false,
}: {
  execution: ExecutionRecord;
  now: number;
  onSelect: (execution: ExecutionRecord) => void;
  nested?: boolean;
  selectedId?: string | null;
  roundOnly?: boolean;
}) {
  const [openError, setOpenError] = useState<string | null>(null);
  return (
    <div className="execution-row-shell">
      <button
        className={`execution-row ${nested ? "nested" : ""} ${execution.id === selectedId ? "selected" : ""} status-${execution.status}`}
        data-execution-id={execution.id}
        title={executionTitle(execution.title)}
        aria-expanded={execution.id === selectedId}
        onClick={() => onSelect(execution)}
      >
        <ClientMark client={execution.client} />
        <span className="execution-main">
          <strong>{execution.isChild ? `子 Agent · 第 ${execution.turnIndex} 轮` : roundOnly ? `第 ${execution.turnIndex} 轮` : executionTitle(execution.title)}</strong>
          <small>
            {execution.isChild && <span>{executionTitle(execution.title)}</span>}
            {roundOnly ? <span>{dateTime(execution.startedAt)}</span> : <><span>{clientNames[execution.client]}</span><span>{projectName(execution.projectPath)}</span><span>第 {execution.turnIndex} 轮</span></>}
            {execution.isChild && (
              <span className="subagent-badge">子 Agent</span>
            )}
            {execution.isChild && execution.rootId === null && <span>所属主轮次未提供</span>}
            {execution.childCount > 0 && (
              <span className="children-count">
                <GitBranch size={10} />
                {execution.childCount}
              </span>
            )}
          </small>
        </span>
        <span className="execution-duration">
          <Clock3 size={12} />
          {formatDuration(elapsed(execution, now))}
        </span>
        <span className="execution-tokens" title="该轮已记录 Token">
          {formatTokens(execution.tokens.total)}
        </span>
        <span className={`task-status ${statusTones[execution.status]}`}>
          <StatusIcon status={execution.status} size={12} />
          {statusLabels[execution.status]}
        </span>
        <ChevronRight size={14} className="row-chevron" />
      </button>
      <span className="execution-source-action">
        <OpenExecutionButton execution={execution} onError={setOpenError} />
      </span>
      {openError !== null && (
        <div className="execution-open-error" role="alert">
          {openError}
        </div>
      )}
    </div>
  );
}

export function ExecutionDetail({
  execution,
  revision,
  now,
  onClose,
  onSelect,
}: {
  execution: ExecutionRecord;
  revision: string | null;
  now: number;
  onClose: () => void;
  onSelect: (execution: ExecutionRecord) => void;
}) {
  const [children, setChildren] = useState<ExecutionRecord[]>([]);
  const [childError, setChildError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [openError, setOpenError] = useState<string | null>(null);
  const childRevision = execution.live ? revision : null;
  useEffect(() => {
    setChildren([]);
    setChildError(null);
    setCopied(false);
    setOpenError(null);
  }, [execution.id]);
  useEffect(() => {
    let disposed = false;
    if (execution.childCount > 0) {
      void invokeCommand<ActivityPage>("get_execution_history", {
        query: {
          rootId: execution.rootId ?? execution.id,
          limit: 200,
          offset: 0,
        },
      })
        .then((page) => {
          if (!disposed) {
            setChildren(
              page.executions.filter((item) => item.id !== execution.id),
            );
            setChildError(null);
          }
        })
        .catch((error) => {
          if (!disposed) setChildError(message(error));
        });
    }
    return () => {
      disposed = true;
    };
  }, [execution.id, execution.rootId, execution.childCount, childRevision]);
  useEffect(() => {
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", close);
    return () => window.removeEventListener("keydown", close);
  }, [onClose]);
  return (
    <aside
      className="detail-panel execution-detail"
      aria-label="执行详情"
      data-execution-id={execution.id}
    >
      <div className="detail-topbar">
        <span className="eyebrow">执行详情</span>
        <button
          className="icon-button"
          onClick={onClose}
          title="关闭详情"
          aria-label="关闭详情"
        >
          <PanelRightClose size={18} />
        </button>
      </div>
      <div className="detail-heading">
        <ClientMark client={execution.client} />
        <div>
          <span className="detail-identity">{clientNames[execution.client]} · {execution.isChild ? "子 Agent" : projectName(execution.projectPath)}</span>
          <h2>第 {execution.turnIndex} 轮执行</h2>
          <p title={execution.projectPath}>{execution.projectPath}</p>
        </div>
      </div>
      <div className="detail-title-context">
        <p>{executionTitleDetails(execution.title)}</p>
        <details><summary>完整来源标题</summary><p>{execution.title}</p></details>
      </div>
      <div className={`detail-state ${statusTones[execution.status]}`}>
        <StatusIcon status={execution.status} />
        <span>{statusLabels[execution.status]}</span>
        <span className="state-reason">
          {execution.attention === "approval"
            ? "等待批准"
            : execution.attention === "user_input"
              ? "等待输入"
              : `第 ${execution.turnIndex} 轮`}
        </span>
      </div>
      <div className="detail-block">
        <div className="detail-label">执行时间</div>
        <dl className="execution-times">
          <div>
            <dt>墙钟耗时</dt>
            <dd>{formatDuration(elapsed(execution, now))}</dd>
          </div>
          <div>
            <dt>活动时间</dt>
            <dd>{formatDuration(execution.activeMs)}</dd>
          </div>
          <div>
            <dt>等待时间</dt>
            <dd>{formatDuration(execution.waitingMs)}</dd>
          </div>
        </dl>
        <div className="detail-grid">
          <span>开始</span>
          <strong>{dateTime(execution.startedAt)}</strong>
          <span>结束</span>
          <strong>{dateTime(execution.completedAt)}</strong>
          <span>观测</span>
          <strong>{dateTime(execution.observedAt)}</strong>
        </div>
      </div>
      <div className="detail-block">
        <div className="detail-label">Token 明细</div>
        <TokenBreakdown usage={execution.tokens} />
      </div>
      <details className="detail-block detail-source">
        <summary className="detail-label">
          执行来源
          <ChevronDown size={13} />
        </summary>
        <div className="detail-grid">
          <span>客户端</span>
          <strong>{clientNames[execution.client]}</strong>
          <span>服务商</span>
          <strong>{execution.provider ?? "未提供"}</strong>
          <span>模型</span>
          <strong>{execution.model ?? "未提供"}</strong>
          <span>会话 ID</span>
          <code title={execution.sessionId}>{execution.sessionId}</code>
          <span>轮次 ID</span>
          <code title={execution.turnId}>{execution.turnId}</code>
        </div>
      </details>
      {execution.isChild && (
        <p className="detail-muted child-notice">
          子 Agent
          {execution.parentId === null ? " · 父轮次未提供" : " · 归属父任务"}
        </p>
      )}
      {execution.childCount > 0 && (
        <div className="detail-block">
          <div className="detail-label">
            <span>子 Agent</span>
            <span>{execution.childCount}</span>
          </div>
          {childError !== null ? (
            <p className="detail-error">{childError}</p>
          ) : children.length === 0 ? (
            <p className="detail-muted">读取中…</p>
          ) : (
            <div className="child-executions">
              {children.map((child) => (
                <button
                  key={child.id}
                  data-execution-id={child.id}
                  onClick={() => onSelect(child)}
                >
                  <span className={statusTones[child.status]}>
                    <StatusIcon status={child.status} size={12} />
                  </span>
                  <span>
                    <strong title={executionTitle(child.title)}>子 Agent · 第 {child.turnIndex} 轮</strong>
                    <small>{executionTitle(child.title)}</small>
                    <small>
                      {formatDuration(elapsed(child, now))} ·{" "}
                      {formatTokens(child.tokens.total)} Token
                    </small>
                  </span>
                  <ChevronRight size={13} />
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      {openError !== null && <p className="detail-error">{openError}</p>}
      <div className="detail-actions">
        <OpenExecutionButton
          key={execution.id}
          execution={execution}
          compact={false}
          onError={setOpenError}
        />
        <button
          className="secondary-button"
          onClick={() =>
            void navigator.clipboard
              .writeText(execution.id)
              .then(() => {
                setCopied(true);
                window.setTimeout(() => setCopied(false), 1500);
              })
              .catch((error) => setOpenError(message(error)))
          }
        >
          {copied ? <Check size={15} /> : <Clipboard size={15} />}
          {copied ? "已复制" : "复制执行 ID"}
        </button>
      </div>
      <div className="detail-footnote">
        <ShieldCheck size={14} />
        本轮结束不代表项目已验收
      </div>
    </aside>
  );
}

export function Overview({
  snapshot,
  loading,
  error,
  now,
  client,
  onSelect,
  onHistory,
  selectedId,
}: {
  snapshot: ActivitySnapshot | null;
  loading: boolean;
  error: string | null;
  now: number;
  client: ClientFilter;
  onSelect: (execution: ExecutionRecord) => void;
  onHistory: () => void;
  selectedId: string | null;
}) {
  const roots = useMemo(
    () =>
      snapshot?.executions.filter(
        (item) => !item.isChild && (client === "all" || item.client === client),
      ) ?? [],
    [snapshot, client],
  );
  const summary = summaryForClient(snapshot, client);
  const current = roots.filter(
    (item) =>
      item.live &&
      (item.status === "running" || item.status === "needs_action"),
  );
  const priority = (execution: ExecutionRecord) =>
    execution.status === "needs_action"
      ? 0
      : execution.status === "running"
        ? 1
        : 2;
  current.sort(
    (a, b) =>
      priority(a) - priority(b) ||
      Date.parse(b.observedAt) - Date.parse(a.observedAt),
  );
  const recent = roots
    .filter(
      (item) =>
        item.status === "succeeded" ||
        item.status === "failed" ||
        item.status === "interrupted",
    )
    .sort((a, b) =>
      a.completedAt === null
        ? b.completedAt === null
          ? 0
          : 1
        : b.completedAt === null
          ? -1
          : Date.parse(b.completedAt) - Date.parse(a.completedAt),
    )
    .slice(0, 8);
  return (
    <div className="overview-workspace">
      <section className="metrics activity-metrics">
        {(
          [
            ["运行中", summary?.running, "blue", Activity],
            ["待处理", summary?.needsAction, "amber", AlertCircle],
            ["今日完成", summary?.completedToday, "green", CheckCircle2],
            ["今日失败", summary?.failedToday, "red", CircleX],
          ] as const
        ).map(([label, value, tone, Icon]) => (
          <div className={`metric ${tone}`} key={label}>
            <span className="metric-label"><Icon size={15} />{label}</span>
            <strong className={`metric-value ${tone}`}>
              {value === undefined ? "—" : value}
            </strong>
          </div>
        ))}
      </section>
      {loading && snapshot === null ? (
        <Empty title="正在读取本机执行记录" loading />
      ) : snapshot === null ? (
        <Empty
          title="暂时无法读取任务"
          detail={error ?? "请检查设置与数据源状态。"}
        />
      ) : (
        <div className="overview-sections">
          {[
            {
              key: "active",
              title: "进行中",
              records: current,
              empty: "当前没有执行中的任务",
            },
            {
              key: "recent",
              title: "最近结束",
              records: recent,
              empty: "暂无结束记录",
            },
          ].map((section) => (
            <section
              className={`overview-executions ${section.key}`}
              key={section.key}
            >
              <div className="activity-section-head">
                <h2>
                  {section.title}
                  <span>{section.records.length}</span>
                </h2>
                {section.key === "active" && (
                  <button className="text-command" onClick={onHistory}>
                    执行历史
                    <ArrowUpRight size={13} />
                  </button>
                )}
              </div>
              {section.records.length === 0 ? (
                <div className="inline-empty">{section.empty}</div>
              ) : (
                <div className="execution-list">
                  <div className="execution-table-heading">
                    <span>任务 / 聊天</span>
                    <span>墙钟耗时</span>
                    <span>已记录 Token</span>
                    <span>状态</span>
                  </div>
                  {section.records.map((execution) => (
                    <ExecutionRow
                      key={execution.id}
                      execution={execution}
                      now={now}
                      onSelect={onSelect}
                      selectedId={selectedId}
                    />
                  ))}
                </div>
              )}
            </section>
          ))}
        </div>
      )}
      {summary !== null && (
        <div className="overview-token-total">
          <span>
            <ChartNoAxesCombined size={14} />
            全历史已记录用量
          </span>
          <strong>{formatTokens(summary.tokens.total)}</strong>
          <span>
            覆盖{" "}
            {summary.tokenCoverage.recordedExecutions.toLocaleString("zh-CN")} /{" "}
            {summary.tokenCoverage.totalExecutions.toLocaleString("zh-CN")}{" "}
            条执行
          </span>
          <span>非缓存输入 {formatTokens(summary.tokens.input)}</span>
          <span>输出 {formatTokens(summary.tokens.output)}</span>
        </div>
      )}
    </div>
  );
}

export function HistoryView({
  revision,
  now,
  client,
  status,
  onStatusChange,
  onSelect,
  selectedId,
}: {
  revision: string | null;
  now: number;
  client: ClientFilter;
  status: "all" | ExecutionStatus;
  onStatusChange: (status: "all" | ExecutionStatus) => void;
  onSelect: (execution: ExecutionRecord) => void;
  selectedId: string | null;
}) {
  const [scope, setScope] = useState<"sessions" | "children">("sessions");
  const [sessionId, setSessionId] = useState("");
  const [appliedSession, setAppliedSession] = useState("");
  const [offset, setOffset] = useState(0);
  const [page, setPage] = useState<SessionPage | null>(null);
  const [childPage, setChildPage] = useState<ActivityPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const limit = 20;
  useEffect(() => setOffset(0), [client]);
  useEffect(() => {
    let disposed = false;
    setLoading(true);
    const query: ActivityQuery = { offset, limit };
    if (client !== "all") query.client = client;
    if (status !== "all") query.status = status;
    if (appliedSession !== "") query.sessionId = appliedSession;
    if (scope === "children") query.childrenOnly = true;
    void invokeCommand<SessionPage | ActivityPage>(scope === "children" ? "get_execution_history" : "get_session_history", { query })
      .then((result) => {
        if (!disposed) {
          if ("sessions" in result) setPage(result);
          else setChildPage(result);
          setError(null);
        }
      })
      .catch((caught) => {
        if (!disposed) setError(message(caught));
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, [client, status, appliedSession, offset, revision, scope]);
  const activePage = scope === "children" ? childPage : page;
  const pageLength = scope === "children" ? childPage?.executions.length : page?.sessions.length;
  return (
    <div className="activity-view history-view">
      <div className="activity-view-heading">
        <div>
          <div className="segmented history-scope" role="group" aria-label="历史范围">
            <button className={scope === "sessions" ? "selected" : ""} aria-pressed={scope === "sessions"} onClick={() => {setScope("sessions");setOffset(0);}}><FolderOpen size={14}/>会话</button>
            <button className={scope === "children" ? "selected" : ""} aria-pressed={scope === "children"} onClick={() => {setScope("children");setOffset(0);}}><GitBranch size={14}/>子 Agent</button>
          </div>
        </div>
        <span className="record-count">
          {scope === "children" ? `${childPage?.total.toLocaleString("zh-CN") ?? "—"} 条子 Agent 执行` : <>{page === null ? "—" : page.total.toLocaleString("zh-CN")} 个会话 · {page?.totalTurns.toLocaleString("zh-CN") ?? "—"} 轮{status === "all" ? "执行" : "匹配"}</>}
        </span>
      </div>
      <div className="history-filters">
        <label>
          <span>状态</span>
          <select
            value={status}
            onChange={(event) => {
              onStatusChange(event.target.value as typeof status);
              setOffset(0);
            }}
          >
            <option value="all">全部状态</option>
            {Object.entries(statusLabels).map(([key, label]) => (
              <option value={key} key={key}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <form
          className="session-search"
          onSubmit={(event) => {
            event.preventDefault();
            setAppliedSession(sessionId);
            setOffset(0);
          }}
        >
          <label>
            <span>会话 ID</span>
            <span className="search-input">
              <Search size={14} />
              <input
                aria-label="会话 ID"
                placeholder="按会话 ID 筛选"
                value={sessionId}
                onChange={(event) => setSessionId(event.target.value)}
              />
              <button type="submit" aria-label="查询会话" title="查询会话">
                <ChevronRight size={15} />
              </button>
            </span>
          </label>
          {appliedSession !== "" && (
            <button
              type="button"
              className="icon-button"
              onClick={() => {
                setSessionId("");
                setAppliedSession("");
                setOffset(0);
              }}
              title="清除会话筛选"
              aria-label="清除会话筛选"
            >
              <XIcon />
            </button>
          )}
        </form>
      </div>
      {error !== null && (
        <div className="inline-alert">
          <AlertCircle size={15} />
          <span>{error}</span>
        </div>
      )}
      {loading && activePage === null ? (
        <Empty title="正在读取历史" loading />
      ) : activePage === null || activePage.total === 0 ? (
        <Empty title="没有匹配的执行记录" />
      ) : scope === "children" ? (
        <div className={`child-history-list ${loading ? "refreshing" : ""}`} aria-label="子 Agent 历史">
          {childPage?.executions.map(execution => <ExecutionRow key={execution.id} execution={execution} now={now} onSelect={onSelect} selectedId={selectedId}/>)}
        </div>
      ) : (
        <div className={`history-groups ${loading ? "refreshing" : ""}`}>
          {page?.sessions.map((session) => <SessionHistoryGroup
            key={`${session.latest.client}:${session.latest.sessionId}:${status}`}
            session={session} status={status} revision={revision} now={now}
            onSelect={onSelect} selectedId={selectedId}
          />)}
        </div>
      )}
      <div className="pagination">
        <span>
          {activePage === null || activePage.total === 0
            ? "0 条"
            : `${offset + 1}–${offset + (pageLength ?? 0)} / ${activePage.total} ${scope === "children" ? "条子 Agent 执行" : "个会话"}`}
        </span>
        <button
          className="icon-button"
          title="上一页"
          aria-label="上一页"
          disabled={offset === 0 || loading}
          onClick={() => setOffset(Math.max(0, offset - limit))}
        >
          <ChevronLeft size={16} />
        </button>
        <button
          className="icon-button"
          title="下一页"
          aria-label="下一页"
          disabled={
            activePage === null ||
            offset + (pageLength ?? 0) >= activePage.total ||
            loading
          }
          onClick={() => setOffset(offset + limit)}
        >
          <ChevronRight size={16} />
        </button>
      </div>
    </div>
  );
}

function SessionHistoryGroup({ session, status, revision, now, onSelect, selectedId }: {
  session: SessionSummary;
  status: "all" | ExecutionStatus;
  revision: string | null;
  now: number;
  onSelect: (record: ExecutionRecord) => void;
  selectedId: string | null;
}) {
  const { latest } = session;
  const [expanded, setExpanded] = useState(false);
  const [offset, setOffset] = useState(0);
  const [rounds, setRounds] = useState<ActivityPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const limit = 10;
  const liveRevision = latest.live ? revision : null;
  useEffect(() => {
    if (!expanded) return;
    let disposed = false;
    setLoading(true);
    const query: ActivityQuery = {client: latest.client, sessionId: latest.sessionId, rootsOnly: true, limit, offset};
    if (status !== "all") query.status = status;
    void invokeCommand<ActivityPage>("get_execution_history", {query}).then(value => {
      if (!disposed) {setRounds(value);setError(null);}
    }).catch(caught => {if (!disposed) setError(message(caught));})
      .finally(() => {if (!disposed) setLoading(false);});
    return () => {disposed = true;};
  }, [expanded, latest.client, latest.sessionId, latest.id, session.turnCount, liveRevision, status, offset]);
  return <section className={`session-history-group ${expanded ? "expanded" : ""}`} data-session-id={`${latest.client}:${latest.sessionId}`}>
    <button className="session-summary" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
      <ClientMark client={latest.client}/>
      <span className="session-summary-copy"><strong title={executionTitle(latest.title)}>{executionTitle(latest.title)}</strong>
        <small><span>{projectName(latest.projectPath)}</span><span>{session.turnCount} 轮{status === "all" ? "执行" : "匹配"}</span>{session.childCount > 0 && <span><GitBranch size={11}/> {session.childCount} 子 Agent</span>}</small>
      </span>
      <span className="session-summary-metrics"><span>{formatTokens(session.tokens.total)} <small>Token</small></span><time>{dateTime(latest.startedAt)}</time></span>
      <span className={`task-status ${statusTones[latest.status]}`}><StatusIcon status={latest.status} size={12}/>{statusLabels[latest.status]}</span>
      <ChevronDown size={15} className="session-chevron"/>
    </button>
    {expanded && <div className="session-rounds" aria-label={`${executionTitle(latest.title)}的执行轮次`}>
      {error !== null ? <div className="inline-alert" role="alert">{error}</div> : rounds === null ? <Empty title="正在读取轮次" loading/> : <>
        <div className={loading ? "refreshing" : ""}>{rounds.executions.map(round => <ExecutionRow key={round.id} execution={round} roundOnly now={now} onSelect={onSelect} selectedId={selectedId}/>)}</div>
        {rounds.total > limit && <div className="pagination round-pagination"><span>{offset + 1}–{offset + rounds.executions.length} / {rounds.total} 轮</span>
          <button className="icon-button" aria-label="较新轮次" title="较新轮次" disabled={offset === 0 || loading} onClick={() => setOffset(Math.max(0,offset-limit))}><ChevronLeft size={15}/></button>
          <button className="icon-button" aria-label="较早轮次" title="较早轮次" disabled={offset+rounds.executions.length>=rounds.total || loading} onClick={() => setOffset(offset+limit)}><ChevronRight size={15}/></button>
        </div>}
      </>}
    </div>}
  </section>;
}

function XIcon() {
  return <X size={14} />;
}

function UsageChart({
  daily,
  dates,
  metric,
  client,
}: {
  daily: DailyUsage[];
  dates: string[];
  metric: TokenMetric;
  client: ClientFilter;
}) {
  const [activeDate, setActiveDate] = useState<string | null>(null);
  const dayButtons = useRef<Array<HTMLButtonElement | null>>([]);
  const visibleClients = client === "all" ? clients : [client];
  const series = dates.map((date) => {
    const values = visibleClients.map((source) => {
      const row = daily.find(
        (item) => item.date === date && item.client === source,
      );
      return row === undefined ? null : row.tokens[metric];
    });
    return { date, values, total: sumRecorded(values) };
  });
  const max = Math.max(
    1,
    ...series
      .map((day) => day.total)
      .filter((value): value is number => value !== null),
  );
  const selectedDate =
    activeDate !== null && dates.includes(activeDate)
      ? activeDate
      : dates[dates.length - 1];
  const selected = series.find((day) => day.date === selectedDate);
  return (
    <>
      <div
        className="usage-chart trend-chart"
        aria-label={`${dates[0]} 至 ${dates[dates.length - 1]} ${metricLabels[metric]}趋势`}
      >
        <div className="chart-scale">
          <span>{formatTokens(max)}</span>
          <span>{formatTokens(max / 2)}</span>
          <span>0</span>
        </div>
        <div
          className="trend-plot"
          style={{
            gridTemplateColumns: `repeat(${dates.length}, minmax(16px, 1fr))`,
          }}
        >
          {series.map((day, index) => {
            const exact = `${day.date} · ${visibleClients.map((source, sourceIndex) => `${clientNames[source]} ${day.values[sourceIndex] === null ? "未提供" : day.values[sourceIndex]?.toLocaleString("zh-CN")}`).join(" · ")}`;
            return (
              <button
                type="button"
                className={`trend-day ${day.date === selectedDate ? "selected" : ""} ${day.total === null ? "missing" : day.total === 0 ? "zero" : ""}`}
                key={day.date}
                ref={(node) => {
                  dayButtons.current[index] = node;
                }}
                tabIndex={day.date === selectedDate ? 0 : -1}
                aria-label={exact}
                aria-pressed={day.date === selectedDate}
                title={exact}
                data-date={day.date}
                onMouseEnter={() => setActiveDate(day.date)}
                onFocus={() => setActiveDate(day.date)}
                onClick={() => setActiveDate(day.date)}
                onKeyDown={(event) => {
                  if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
                    event.preventDefault();
                    dayButtons.current[
                      Math.max(
                        0,
                        Math.min(
                          dates.length - 1,
                          index + (event.key === "ArrowLeft" ? -1 : 1),
                        ),
                      )
                    ]?.focus();
                  }
                }}
              >
                <span className="trend-column">
                  <span
                    className="trend-stack"
                    style={{
                      height: `${day.total === null ? 0 : (day.total / max) * 100}%`,
                    }}
                  >
                    {day.values.map((value, sourceIndex) =>
                      value === null || value === 0 ? null : (
                        <i
                          key={visibleClients[sourceIndex]}
                          className={visibleClients[sourceIndex]}
                          style={{
                            height: `${day.total === null || day.total === 0 ? 0 : (value / day.total) * 100}%`,
                          }}
                        />
                      ),
                    )}
                  </span>
                  {day.total === null && <span className="trend-missing" />}
                  {day.total === 0 && <span className="trend-zero" />}
                </span>
                <span className="trend-tick">
                  {dates.length <= 14
                    ? index % 2 === 0 || index === dates.length - 1
                      ? day.date.slice(8)
                      : ""
                    : index % 5 === 0 || index === dates.length - 1
                      ? day.date.slice(8)
                      : ""}
                </span>
              </button>
            );
          })}
        </div>
      </div>
      <div className="chart-legend trend-legend">
        {visibleClients.map((source) => (
          <span key={source}>
            <i className={source} />
            {clientNames[source]}
          </span>
        ))}
        <span>
          <i className="missing" />
          未提供
        </span>
      </div>
      {selected !== undefined && (
        <div className="usage-hover" aria-live="polite">
          <time dateTime={selected.date}>
            {selected.date.replaceAll("-", "/")}
          </time>
          {visibleClients.map((source, index) => (
            <div key={source}>
              <span className={`usage-source-label ${source}`}>
                {clientNames[source]}
              </span>
              <strong>
                {selected.values[index] === null
                  ? "未提供"
                  : selected.values[index]?.toLocaleString("zh-CN")}
              </strong>
            </div>
          ))}
        </div>
      )}
    </>
  );
}

export function UsageView({
  snapshot,
  now,
  client,
  onClientChange,
  range,
  onRangeChange,
  metric,
  onMetricChange,
}: {
  snapshot: ActivitySnapshot | null;
  now: number;
  client: ClientFilter;
  onClientChange: (client: ClientFilter) => void;
  range: UsageRange;
  onRangeChange: (range: UsageRange) => void;
  metric: TokenMetric;
  onMetricChange: (metric: TokenMetric) => void;
}) {
  const today = calendarDates(new Date(now).toISOString(), 1)[0];
  const isCustom = typeof range === "object";
  const initialDates = calendarDates(new Date(now).toISOString(), 14);
  const [customFrom, setCustomFrom] = useState(isCustom ? range.from : initialDates[0]);
  const [customThrough, setCustomThrough] = useState(isCustom ? range.through : today);
  const dates = useMemo(() => typeof range === "object"
    ? datesInRange(range.from, range.through)
    : calendarDates(`${today}T12:00:00`, range), [today, range]);
  const validCustom = customFrom !== "" && customThrough !== "" && customFrom <= customThrough;
  const allRows =
    snapshot?.daily.filter(
      (row) => row.date >= dates[0] && row.date <= dates[dates.length - 1],
    ) ?? [];
  const rows = allRows.filter(
    (row) => client === "all" || row.client === client,
  );
  const totals = dailyTokenUsage(rows);
  const coverage = summaryForClient(snapshot, client)?.tokenCoverage ?? null;
  const comparison = clients.map((source) => ({
    client: source,
    value: sumRecorded(
      allRows
        .filter((row) => row.client === source)
        .map((row) => row.tokens[metric]),
    ),
  }));
  const comparisonMax = Math.max(
    1,
    ...comparison
      .map((source) => source.value)
      .filter((value): value is number => value !== null),
  );
  const recordedDays = dates.filter((date) =>
    rows.some((row) => row.date === date && row.tokens[metric] !== null),
  ).length;
  return (
    <div className="activity-view usage-view">
      {snapshot === null ? (
        <Empty title="等待用量记录" />
      ) : (
        <>
          <div className="usage-toolbar">
            <span className="usage-period">
              <CalendarDays size={14} />
              {dates[0].replaceAll("-", "/")} 至{" "}
              {dates[dates.length - 1].replaceAll("-", "/")}
            </span>
            <div className="usage-controls">
              <div
                className="segmented range-control"
                role="group"
                aria-label="用量日期范围"
              >
                {([1, 7, 14, 30] as const).map((days) => (
                  <button
                    key={days}
                    className={range === days ? "selected" : ""}
                    aria-pressed={range === days}
                    onClick={() => onRangeChange(days)}
                  >
                    {days === 1 ? "今天" : `${days} 天`}
                  </button>
                ))}
                <button className={isCustom ? "selected" : ""} aria-pressed={isCustom}
                  onClick={() => {
                    setCustomFrom(dates[0]);
                    setCustomThrough(dates[dates.length - 1]);
                    onRangeChange({ from: dates[0], through: dates[dates.length - 1] });
                  }}>
                  自定义
                </button>
              </div>
              <select
                aria-label="Token 分类"
                value={metric}
                onChange={(event) =>
                  onMetricChange(event.target.value as TokenMetric)
                }
              >
                {Object.entries(metricLabels).map(([key, label]) => (
                  <option key={key} value={key}>
                    {label}
                  </option>
                ))}
              </select>
            </div>
          </div>
          {isCustom && <form className="usage-date-range" onSubmit={(event) => {
            event.preventDefault();
            if (validCustom) onRangeChange({ from: customFrom, through: customThrough });
          }}>
            <label>开始日期<input type="date" aria-label="用量开始日期" value={customFrom}
              onChange={(event) => setCustomFrom(event.target.value)} /></label>
            <span>至</span>
            <label>结束日期<input type="date" aria-label="用量结束日期" value={customThrough}
              onChange={(event) => setCustomThrough(event.target.value)} /></label>
            <button type="submit" className="secondary-button" disabled={!validCustom}>应用范围</button>
            {!validCustom && <span className="usage-range-error" role="alert">请选择有效日期，开始日期不能晚于结束日期。</span>}
          </form>}
          <section className="usage-totals">
            <TokenBreakdown usage={totals} activeMetric={metric} />
          </section>
          <div className="usage-coverage">
            <span>
              所选分类有记录 {recordedDays} / {dates.length} 天
            </span>
            {coverage !== null && (
              <span>
                全历史用量覆盖{" "}
                {coverage.recordedExecutions.toLocaleString("zh-CN")} /{" "}
                {coverage.totalExecutions.toLocaleString("zh-CN")} 条执行
              </span>
            )}
          </div>
          <p className="usage-accounting-note">Token 按请求记录时间归入本机日期；包含已归档、已删除聊天的历史消耗。未提供的数据不估算，推理不重复计入总量。</p>
          <div className="usage-workspace">
            <section className="usage-trend">
              <div className="activity-section-head">
                <h2>每日趋势</h2>
                <span className="record-count">{metricLabels[metric]}</span>
              </div>
              <UsageChart
                daily={rows}
                dates={dates}
                metric={metric}
                client={client}
              />
            </section>
            <aside className="usage-comparison">
              <h2>来源对比</h2>
              <span className="comparison-scope">同日期范围 · 全客户端</span>
              {comparison.map((source) => (
                <button
                  className={`source-comparison-row ${client === source.client ? "selected" : ""}`}
                  key={source.client}
                  onClick={() => onClientChange(source.client)}
                  aria-label={`对比筛选${clientNames[source.client]}`}
                  aria-pressed={client === source.client}
                >
                  <span className="comparison-row-heading">
                    <span className="client-name">
                      <ClientMark client={source.client} />
                      {clientNames[source.client]}
                    </span>
                    <strong
                      title={
                        source.value === null
                          ? undefined
                          : source.value.toLocaleString("zh-CN")
                      }
                    >
                      {formatTokens(source.value)}
                    </strong>
                  </span>
                  <span
                    className={`comparison-track ${source.value === null ? "missing" : ""}`}
                  >
                    <i
                      className={source.client}
                      style={{
                        width: `${source.value === null ? 0 : (source.value / comparisonMax) * 100}%`,
                      }}
                    />
                  </span>
                </button>
              ))}
              <p className="comparison-note">
                {metric === "reasoning"
                  ? "推理是输出细分，不重复计入总量。"
                  : "已记录用量；未提供的数据不计入。"}
              </p>
            </aside>
          </div>
          <details className="usage-records">
            <summary>
              <span>
                每日记录 <small>{rows.length}</small>
              </span>
              <ChevronDown size={14} />
            </summary>
            <div className="usage-data-table">
              <div className="usage-data-heading">
                <span>日期</span>
                <span>客户端</span>
                <span>主任务</span>
                <span>已完成</span>
                <span>{metricLabels[metric]}</span>
              </div>
              {rows
                .slice()
                .sort(
                  (a, b) =>
                    b.date.localeCompare(a.date) ||
                    a.client.localeCompare(b.client),
                )
                .map((row) => (
                  <div
                    className="usage-data-row"
                    key={`${row.date}:${row.client}`}
                  >
                    <time dateTime={row.date}>{row.date}</time>
                    <span className="client-name">
                      <ClientMark client={row.client} />
                      {clientNames[row.client]}
                    </span>
                    <span>{row.turns}</span>
                    <span>{row.completed}</span>
                    <strong
                      title={
                        row.tokens[metric] === null
                          ? undefined
                          : row.tokens[metric]?.toLocaleString("zh-CN")
                      }
                    >
                      {formatTokens(row.tokens[metric])}
                    </strong>
                  </div>
                ))}
            </div>
          </details>
        </>
      )}
    </div>
  );
}
