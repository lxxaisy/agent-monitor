import { useEffect, useMemo, useState } from "react";
import {
  Activity,
  AlertCircle,
  Bell,
  Bluetooth,
  Check,
  CheckCircle2,
  ChevronDown,
  Clock3,
  ExternalLink,
  LoaderCircle,
  Plug,
  RefreshCw,
  Search,
  ShieldCheck,
  TerminalSquare,
  Usb,
  Wallet,
  ArrowUpRight,
  Radio,
} from "lucide-react";
import { invokeCommand, isNativeApp } from "./lib/tauri";
import { executionTitle } from "./lib/display-text";
import {
  Empty,
  Toggle,
  dateTime,
  localDateTimeSeconds,
  localTimeZone,
  message,
  quotaLabels,
  relativeTime,
} from "./activity-ui";
import type {
  AlertRecord,
  Diagnostics,
  PassportSettings,
  PassportState,
  PassportTarget,
  QuotaSnapshot,
  QuotaWindow,
  Settings,
  NewsSnapshot,
  NewsSettings,
  ResetPost,
} from "./types";

export const resetStateLabels: Record<string,string> = {announced:"公告待确认",confirmed:"原帖称已重置",banked:"重置卡消息"};

function resetTime(post: ResetPost): string {
  return localDateTimeSeconds(post.timingAt, post.timingKind === "deadline" ? " 前" : "");
}

function resetTimeLabel(post: ResetPost): string {
  if (post.timingKind === "deadline") return "预计最晚重置";
  if (post.timingKind === "window") return "预计时间窗口截止";
  return "公告预计重置时间";
}

function decodeTextEntities(text: string): string {
  if (!text.includes("&")) return text;
  const document = new DOMParser().parseFromString(text.replace(/</g, "&lt;"), "text/html");
  return document.body.textContent ?? "";
}

export function ResetNewsSpotlight({ news, onOpen }: { news: NewsSnapshot | null; onOpen: () => void }) {
  const latest = news?.posts[0];
  if (latest === undefined) return null;
  return <button className={`news-spotlight ${news?.state === "available" ? "" : "source-unavailable"}`} onClick={onOpen} aria-label="查看 Tibo 重置动态">
    <span className="news-signal"><Radio size={19}/></span>
    <span className="news-spotlight-copy">
      <span className="news-spotlight-label">Tibo · Codex / ChatGPT <span>{resetStateLabels[latest.state]}</span></span>
      <strong lang="en">{decodeTextEntities(latest.text)}</strong>
      {typeof latest.timingAt === "number" && <span className="news-spotlight-timing"><Clock3 size={14} />{resetTimeLabel(latest)} · {resetTime(latest)}</span>}
      <small>{localDateTimeSeconds(latest.publishedAt)} 发布{news?.state === "available" ? "" : " · 来源未更新，保留上次消息"}</small>
    </span>
    <ArrowUpRight size={18}/>
  </button>;
}

export function ResetNewsSection({ news, onNews, loadError }: {
  news: NewsSnapshot | null;
  onNews: (news: NewsSnapshot) => void;
  loadError: string | null;
}) {
  const [draft,setDraft]=useState<NewsSettings>({enabled:true,intervalMinutes:5});
  const [busy,setBusy]=useState(false);
  const [error,setError]=useState<string|null>(null);
  const [expanded,setExpanded]=useState(false);
  useEffect(()=>{if(news!==null)setDraft(news.settings);},[news?.settings.enabled,news?.settings.intervalMinutes]);
  const run=async(command:"refresh_reset_news"|"update_reset_news")=>{
    setBusy(true);setError(null);
    try {const value=await invokeCommand<NewsSnapshot>(command,command==="update_reset_news"?{settings:draft}:undefined);onNews(value);setDraft(value.settings);}
    catch(e){setError(message(e));}finally{setBusy(false);}
  };
  const openPost = (id: string, url: string) => {
    if(isNativeApp())void invokeCommand("open_reset_post",{id}).catch(e=>setError(message(e)));
    else window.open(url,"_blank","noopener,noreferrer");
  };
  const latest = news?.posts[0];
  return (
    <section className="activity-view reset-news-section" aria-label="Tibo 重置消息">
      <div className="activity-view-heading">
        <div><h2>Tibo · Codex / ChatGPT</h2><p>@thsottiaux · X 公开动态 · 第三方转录</p></div>
        <button className="secondary-button" disabled={busy || news?.settings.enabled === false} onClick={() => void run("refresh_reset_news")}>
          <RefreshCw size={14} className={busy ? "spin" : ""} />检查消息
        </button>
      </div>
      <p className="news-account-boundary"><ShieldCheck size={15} />公告不代表你的账户已重置 · BigModel 国内账户独立统计</p>
      {(error ?? loadError) !== null && <div className="inline-alert" role="alert">{error ?? loadError}</div>}
      <div className="news-layout">
        <div className="news-stream">
          {latest !== undefined && (
            <article className={`news-feature ${latest.state}`}>
              <div className="news-feature-heading">
                <h3><Radio size={16} />最新公告</h3>
                <span className={`news-state ${latest.state}`}>{resetStateLabels[latest.state]}</span>
              </div>
              <blockquote lang="en">{decodeTextEntities(latest.text)}</blockquote>
              {typeof latest.timingAt === "number" && (
                <div className="news-schedule">
                  <Clock3 size={18} />
                  <div><span>{resetTimeLabel(latest)} · 本地时间</span><strong>{resetTime(latest)}</strong><small>公告预计时间，实际完成以账户额度为准</small></div>
                </div>
              )}
              {latest.timingAt === null && <p className="news-timing-unavailable">{latest.timingText}</p>}
              {latest.timingSourceId !== null && latest.timingSourceId !== latest.id && (
                <div className="news-reply"><span>回复时间原文</span><p lang="en">{decodeTextEntities(latest.timingText)}</p></div>
              )}
              <div className="news-feature-footer">
                <time>{localDateTimeSeconds(latest.publishedAt)} 发布</time>
                <button className="text-command" onClick={() => openPost(latest.id, latest.url)}><ExternalLink size={14} />查看 X 原帖</button>
              </div>
            </article>
          )}
          {news === null ? <Empty title={loadError === null ? "正在读取消息记录" : "消息记录暂不可用"} /> : news.posts.length === 0 ? <Empty title="尚无重置消息" /> : news.posts.length > 1 && (
            <section className="reset-posts" aria-label="公告历史">
              <h3 className="news-history-heading">公告历史 <span>{news.posts.length - 1} 条</span></h3>
              {(expanded ? news.posts.slice(1) : news.posts.slice(1, 4)).map(post => (
                <article className="reset-post" key={post.id}>
                  <div className="reset-post-heading">
                    <span className={`news-state ${post.state}`}>{resetStateLabels[post.state]}</span>
                    <time>{localDateTimeSeconds(post.publishedAt)} 发布</time>
                  </div>
                  <p className="reset-original" lang="en">{decodeTextEntities(post.text)}</p>
                  {typeof post.timingAt === "number" && <p className="news-time-local"><Clock3 size={14} /><strong>{resetTimeLabel(post)} · {resetTime(post)}</strong></p>}
                  {post.timingSourceId !== null && post.timingSourceId !== post.id && <div className="news-reply"><span>回复时间原文</span><p lang="en">{decodeTextEntities(post.timingText)}</p></div>}
                  <button className="text-command" onClick={() => openPost(post.id, post.url)}><ExternalLink size={13} />查看 X 原帖</button>
                </article>
              ))}
              {news.posts.length > 4 && <button className="secondary-button news-history-toggle" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}><ChevronDown size={14} />{expanded ? "收起历史" : `查看全部 ${news.posts.length - 1} 条历史`}</button>}
            </section>
          )}
        </div>
        <aside className="news-monitor" aria-label="消息检查与来源">
          <h3>消息检查</h3>
          {news !== null && <>
            <span className={`availability ${news.state}`}>{({ available: "来源可用", disabled: "已暂停", stale: "来源过期", error: "检查失败", unavailable: "等待检查" } as Record<string, string>)[news.state] ?? news.state}</span>
            <dl className="news-check-times">
              <div><dt>最近检查</dt><dd>{news.checkedAt === null ? "尚未检查" : localDateTimeSeconds(news.checkedAt)}</dd></div>
              <div><dt>下次检查</dt><dd>{news.settings.enabled ? (news.nextCheckAt === 0 ? "等待检查" : localDateTimeSeconds(news.nextCheckAt)) : "不自动检查"}</dd></div>
            </dl>
          </>}
          <details className="reset-preferences">
            <summary>检查设置与来源<ChevronDown size={14} /></summary>
            <div className="reset-controls">
              <label><input type="checkbox" checked={draft.enabled} onChange={e => setDraft({ ...draft, enabled: e.target.checked })} />自动检查</label>
              <label>间隔<select aria-label="公告检查间隔" value={draft.intervalMinutes} onChange={e => setDraft({ ...draft, intervalMinutes: Number(e.target.value) })}>{Array.from({ length: 60 }, (_, i) => i + 1).map(n => <option key={n} value={n}>{n} 分钟</option>)}</select></label>
              <button className="secondary-button" disabled={busy} onClick={() => void run("update_reset_news")}><Check size={14} />保存</button>
            </div>
            <p>{news?.detail ?? "首次检查仅回填，之后的新消息和修改会提醒。"}</p>
            <p className="settings-note">来源 codex-reset.com，原帖来自 X。时间按本机时区 {localTimeZone()} 显示。</p>
          </details>
        </aside>
      </div>
    </section>
  );
}

export const passportLabels: Record<PassportState["state"], string> = {
  disconnected: "未连接",
  connecting: "连接中",
  connected: "已连接",
  error: "连接失败",
};
function clock(minutes: number): string {
  return `${String(Math.floor(minutes / 60)).padStart(2, "0")}:${String(minutes % 60).padStart(2, "0")}`;
}
function minuteOfDay(value: string): number {
  const [hour, minute] = value.split(":").map(Number);
  return hour * 60 + minute;
}

export function QuotasView({
  snapshot,
  loading,
  onRefresh,
}: {
  snapshot: QuotaSnapshot | null;
  loading: boolean;
  onRefresh: () => void;
}) {
  const groups = useMemo(() => {
    const result = new Map<string, QuotaWindow[]>();
    snapshot?.windows.forEach((window) => {
      const key = `${window.provider}:${window.accountLabel}`;
      result.set(key, [...(result.get(key) ?? []), window]);
    });
    return [...result.entries()];
  }, [snapshot]);
  return (
    <div className="activity-view quotas-view">
      <div className="activity-view-heading">
        <div>
          <h2>
            {snapshot === null ? "服务账户" : `${groups.length} 个服务账户`}
          </h2>
          <p>
            {snapshot === null
              ? "未同步"
              : `${relativeTime(snapshot.generatedAt)}同步 · ${localTimeZone()}`}
          </p>
        </div>
        <button
          className="secondary-button"
          onClick={onRefresh}
          disabled={loading}
        >
          <RefreshCw size={14} className={loading ? "spin" : ""} />
          刷新
        </button>
      </div>
      {snapshot === null ? (
        <Empty
          title={loading ? "正在读取账户额度" : "尚未取得额度数据"}
          loading={loading}
        />
      ) : groups.length === 0 ? (
        <Empty title="暂无账户额度" />
      ) : (
        <div className="quota-groups">
          {groups.map(([key, windows]) => (
            <section className="quota-group" key={key}>
              <header className="quota-account">
                <span className="quota-account-icon"><Wallet size={18} /></span>
                <div>
                  <h3>{windows[0].accountLabel}</h3>
                  <p>{windows[0].provider}</p>
                </div>
              </header>
              <div className="quota-windows">{windows.map((window) => (
                <div className={`quota-window ${window.state}`} key={window.id}>
                  <div className="quota-window-heading">
                    <strong>{window.label}</strong>
                    <span className={`availability ${window.state}`}>
                      {quotaLabels[window.state]}
                    </span>
                  </div>
                  {window.usedPercent === null ? (
                    <div className="quota-empty-value">未提供</div>
                  ) : (
                    <div className="quota-value">
                      <strong>
                        {window.usedPercent.toLocaleString("zh-CN", {
                          maximumFractionDigits: 1,
                        })}
                        <small>%</small>
                      </strong>
                      <span>已用</span>
                    </div>
                  )}
                  <div
                    className="quota-bar"
                    aria-label={
                      window.usedPercent === null
                        ? "额度未提供"
                        : `已用 ${window.usedPercent}%`
                    }
                  >
                    {window.usedPercent !== null && (
                      <span
                        style={{
                          width: `${Math.max(0, Math.min(100, window.usedPercent))}%`,
                        }}
                      />
                    )}
                  </div>
                  <dl className="quota-meta">
                    <div><dt>{window.state==="stale"?"上次剩余":"剩余额度"}</dt><dd>{window.usedPercent===null?"未提供":`${(100-window.usedPercent).toLocaleString("zh-CN",{maximumFractionDigits:1})}%`}</dd></div>
                    <div><dt>统计窗口</dt><dd>
                      {window.windowMinutes === null
                        ? "窗口未提供"
                        : window.windowMinutes >= 1440
                          ? `${window.windowMinutes / 1440} 天窗口`
                          : `${window.windowMinutes / 60} 小时窗口`}
                    </dd></div>
                    <div className="quota-reset"><dt>下次重置 · 本地时间</dt><dd>
                      {window.resetsAt === null
                        ? "重置时间未提供"
                        : `${localDateTimeSeconds(window.resetsAt)} 重置${window.resetsAt*1000<=Date.now()?" · 已到时间，等待账户刷新":""}`}
                    </dd></div>
                  </dl>
                  <footer className="quota-window-footer">
                    <p className="quota-detail">{window.detail}</p>
                    <small className="quota-observed"><Clock3 size={12} />
                    {window.observedAt === null
                      ? "尚未取得账户快照"
                      : `${relativeTime(new Date(window.observedAt * 1000).toISOString())}同步`}
                    </small>
                  </footer>
                </div>
              ))}</div>
            </section>
          ))}
        </div>
      )}
    </div>
  );
}

export function PassportView({
  state,
  onState,
  onError,
}: {
  state: PassportState | null;
  onState: (state: PassportState) => void;
  onError: (error: string) => void;
}) {
  const [transport, setTransport] = useState<"usb" | "ble">("usb");
  const [targets, setTargets] = useState<PassportTarget[]>([]);
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState<PassportSettings>({
    muted: false,
    quietStart: 1320,
    quietEnd: 480,
  });
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    if (state !== null) {
      setDraft(state.settings);
      if (state.transport !== null) setTransport(state.transport);
      if (state.target !== null) setTarget(state.target);
    }
  }, [
    state?.settings.muted,
    state?.settings.quietStart,
    state?.settings.quietEnd,
    state?.transport,
    state?.target,
  ]);
  const scan = async () => {
    setBusy(true);
    try {
      const result = await invokeCommand<PassportTarget[]>(
        transport === "usb" ? "list_passport_usb_ports" : "scan_passport_ble",
      );
      setTargets(result);
      if (!result.some((item) => item.id === target)) setTarget("");
    } catch (error) {
      onError(message(error));
    } finally {
      setBusy(false);
    }
  };
  const connect = async () => {
    setBusy(true);
    try {
      onState(
        await invokeCommand<PassportState>("connect_passport", {
          transport,
          target,
        }),
      );
    } catch (error) {
      onError(message(error));
    } finally {
      setBusy(false);
    }
  };
  const disconnect = async () => {
    setBusy(true);
    try {
      onState(await invokeCommand<PassportState>("disconnect_passport"));
    } catch (error) {
      onError(message(error));
    } finally {
      setBusy(false);
    }
  };
  const save = async () => {
    setBusy(true);
    try {
      onState(
        await invokeCommand<PassportState>("update_passport_settings", {
          settings: draft,
        }),
      );
      setSaved(true);
      window.setTimeout(() => setSaved(false), 1800);
    } catch (error) {
      onError(message(error));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="activity-view passport-view">
      <div className="activity-view-heading">
        <div>
          <strong className="section-title-text">
            {state?.deviceName ?? "未读取设备"}
          </strong>
        </div>
        <span
          className={`availability ${state?.state === "connected" ? "available" : "unavailable"}`}
        >
          {state === null ? "未读取" : passportLabels[state.state]}
        </span>
      </div>
      <section className="passport-connection">
        <div className="passport-connection-controls">
          <div className="segmented transport-tabs">
            <button
              className={transport === "usb" ? "selected" : ""}
              onClick={() => {
                setTransport("usb");
                setTargets([]);
                setTarget("");
              }}
            >
              <Usb size={14} />
              USB
            </button>
            <button
              className={transport === "ble" ? "selected" : ""}
              onClick={() => {
                setTransport("ble");
                setTargets([]);
                setTarget("");
              }}
            >
              <Bluetooth size={14} />
              蓝牙
            </button>
          </div>
          <label className="field">
            <span>{transport === "usb" ? "设备串口" : "蓝牙设备"}</span>
            <div className="target-picker">
              <select
                value={target}
                onChange={(event) => setTarget(event.target.value)}
                aria-label={transport === "usb" ? "设备串口" : "蓝牙设备"}
              >
                <option value="">选择设备</option>
                {targets.map((item) => (
                  <option value={item.id} key={item.id}>
                    {item.name}
                  </option>
                ))}
                {state?.target !== null &&
                  state?.target !== undefined &&
                  state.transport === transport &&
                  !targets.some((item) => item.id === state.target) && (
                    <option value={state.target}>{state.target}</option>
                  )}
              </select>
              <button
                className="icon-button"
                onClick={() => void scan()}
                disabled={busy}
                title={transport === "usb" ? "扫描串口" : "扫描蓝牙"}
                aria-label={transport === "usb" ? "扫描串口" : "扫描蓝牙"}
              >
                <Search size={15} className={busy ? "spin" : ""} />
              </button>
            </div>
          </label>
          <div className="connection-actions">
            <button
              className="primary-button"
              onClick={() => void connect()}
              disabled={busy || target === "" || state?.state === "connecting"}
            >
              <Plug size={14} />
              连接
            </button>
            <button
              className="secondary-button"
              onClick={() => void disconnect()}
              disabled={busy || state === null || state.state === "disconnected"}
            >
              {state?.state === "connecting"
                ? "取消连接"
                : state?.state === "error"
                  ? "停止重试"
                  : "断开"}
            </button>
          </div>
          {state?.error !== null && state?.error !== undefined && (
            <p className="detail-error">{state.error}</p>
          )}
        </div>
      </section>
      <dl className="passport-diagnostics">
        <div>
          <dt>设备</dt>
          <dd>{state?.deviceName ?? "未读取"}</dd>
        </div>
        <div>
          <dt>固件</dt>
          <dd>{state?.firmware ?? "未读取"}</dd>
        </div>
        <div>
          <dt>传输</dt>
          <dd>
            {state?.transport === null || state?.transport === undefined
              ? "未连接"
              : state.transport.toUpperCase()}
          </dd>
        </div>
        <div>
          <dt>最近同步</dt>
          <dd>{relativeTime(state?.lastSyncAt ?? null)}</dd>
        </div>
      </dl>
      {(state?.firmware === "0.2.0" || state?.firmware === "0.2.1") && <p className="settings-note">重置公告与对应设备提醒需要固件 0.2.2。当前固件继续同步任务和账户额度。</p>}
      <section className="notification-settings">
        <div className="activity-section-head">
          <h3>声音与静音时段</h3>
          <button
            className="secondary-button"
            onClick={() => void save()}
            disabled={busy}
          >
            <Check size={14} />
            {saved ? "已保存" : "保存"}
          </button>
        </div>
        <Toggle
          label="手动静音"
          checked={draft.muted}
          onChange={(muted) => setDraft({ ...draft, muted })}
        />
        <div className="quiet-hours">
          <label className="field">
            <span>静音开始</span>
            <input
              type="time"
              value={clock(draft.quietStart)}
              onChange={(event) => {
                if (event.target.value !== "")
                  setDraft({
                    ...draft,
                    quietStart: minuteOfDay(event.target.value),
                  });
              }}
            />
          </label>
          <label className="field">
            <span>静音结束</span>
            <input
              type="time"
              value={clock(draft.quietEnd)}
              onChange={(event) => {
                if (event.target.value !== "")
                  setDraft({
                    ...draft,
                    quietEnd: minuteOfDay(event.target.value),
                  });
              }}
            />
          </label>
        </div>
        <p className="settings-note">
          静音时段仍亮屏并保留未读提醒。Mac 睡眠或断线后，设备显示最近快照。
        </p>
      </section>
    </div>
  );
}

export function AlertsView({
  alerts,
  onAck,
  onSelect,
}: {
  alerts: AlertRecord[];
  onAck: (id: string) => Promise<void>;
  onSelect: (id: string) => void;
}) {
  const [unreadOnly, setUnreadOnly] = useState(true);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const visible = alerts.filter((alert) => !unreadOnly || alert.unread);
  const ack = async (id: string) => {
    setPending(id);
    try {
      await onAck(id);
      setError(null);
    } catch (error) {
      setError(message(error));
    } finally {
      setPending(null);
    }
  };
  return (
    <div className="activity-view">
      <div className="activity-view-heading">
        <div>
          <strong className="section-title-text">
            {alerts.filter((alert) => alert.unread).length} 条未读
          </strong>
          <p>关闭浮窗不等于已读。提醒记录发生时的事件，当前状态见详情。</p>
        </div>
        <div className="segmented">
          <button
            className={unreadOnly ? "selected" : ""}
            onClick={() => setUnreadOnly(true)}
          >
            未读 {alerts.filter((alert) => alert.unread).length}
          </button>
          <button
            className={!unreadOnly ? "selected" : ""}
            onClick={() => setUnreadOnly(false)}
          >
            全部
          </button>
        </div>
      </div>
      {error !== null && (
        <div className="inline-alert">
          <AlertCircle size={15} />
          <span>{error}</span>
        </div>
      )}
      {visible.length === 0 ? (
        <Empty title={unreadOnly ? "没有未读提醒" : "暂无提醒记录"} />
      ) : (
        <div className="alert-list">
          {visible.map((alert) => (
            <div
              className={`alert-row alert-${alert.kind} ${alert.unread ? "unread" : ""}`}
              key={alert.id}
            >
              <span
                className={`alert-event-icon ${alert.kind === "completed" ? "green" : alert.kind === "failed" ? "red" : "amber"}`}
              >
                {alert.kind === "completed" ? (
                  <CheckCircle2 size={17} />
                ) : (
                  <AlertCircle size={17} />
                )}
              </span>
              <button
                className="alert-event-copy"
                onClick={() => onSelect(alert.taskId)}
              >
                <strong>{executionTitle(alert.title)}</strong>
                <small>
                  {alert.kind === "completed"
                    ? "已完成"
                    : alert.kind === "failed"
                      ? "失败"
                      : alert.kind === "reset_news" ? "重置消息"
                      : "需要处理"}{" "}
                  · {dateTime(alert.createdAt)}
                </small>
              </button>
              {alert.unread ? (
                <button
                  className="icon-button"
                  onClick={() => void ack(alert.id)}
                  disabled={pending === alert.id}
                  title="标为已读"
                  aria-label="标为已读"
                >
                  {pending === alert.id ? (
                    <LoaderCircle size={15} className="spin" />
                  ) : (
                    <Check size={15} />
                  )}
                </button>
              ) : (
                <span className="read-marker">已读</span>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export function SettingsView({
  settings,
  diagnostics,
  onSave,
}: {
  settings: Settings;
  diagnostics: Diagnostics | null;
  onSave: (settings: Settings) => Promise<void>;
}) {
  const [draft, setDraft] = useState(settings);
  const [durationMinutes, setDurationMinutes] = useState(
    String(settings.notificationDurationSeconds / 60),
  );
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setDraft(settings);
    setDurationMinutes(String(settings.notificationDurationSeconds / 60));
  }, [settings]);
  const durationValue = Number(durationMinutes);
  const durationValid =
    durationMinutes !== "" &&
    Number.isInteger(durationValue) &&
    durationValue >= 1 &&
    durationValue <= 60;
  const save = async () => {
    if (!durationValid) return;
    setSaving(true);
    try {
      await onSave({
        ...draft,
        notificationDurationSeconds: durationValue * 60,
        showPromptPreview: false,
      });
      setSaved(true);
      setError(null);
      window.setTimeout(() => setSaved(false), 1800);
    } catch (error) {
      setError(message(error));
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="settings-view">
      <div className="view-heading">
        <span className="section-title-text">连接与偏好</span>
        <button
          className="primary-button"
          onClick={() => void save()}
          disabled={saving || !durationValid}
        >
          {saving ? (
            <LoaderCircle size={14} className="spin" />
          ) : (
            <Check size={14} />
          )}
          {saved ? "已保存" : "保存设置"}
        </button>
      </div>
      {error !== null && (
        <div className="inline-alert">
          <AlertCircle size={15} />
          <span>{error}</span>
        </div>
      )}
      <div className="settings-grid">
        <section className="settings-section">
          <div className="section-title">
            <TerminalSquare size={17} />
            <div>
              <h2>Codex 连接</h2>
              <p>现有本机 app-server 连接。</p>
            </div>
          </div>
          <label className="field">
            <span>Codex 可执行文件</span>
            <input
              value={draft.codexPath ?? ""}
              placeholder="自动发现 codex"
              onChange={(event) =>
                setDraft({
                  ...draft,
                  codexPath:
                    event.target.value === "" ? null : event.target.value,
                })
              }
            />
          </label>
          <label className="field">
            <span>CODEX_HOME</span>
            <input
              value={draft.codexHome ?? ""}
              placeholder="~/.codex"
              onChange={(event) =>
                setDraft({
                  ...draft,
                  codexHome:
                    event.target.value === "" ? null : event.target.value,
                })
              }
            />
          </label>
          <label className="field">
            <span>界面刷新间隔</span>
            <select
              value={draft.refreshIntervalSeconds}
              onChange={(event) =>
                setDraft({
                  ...draft,
                  refreshIntervalSeconds: Number(event.target.value),
                })
              }
            >
              <option value={5}>5 秒</option>
              <option value={15}>15 秒</option>
              <option value={30}>30 秒</option>
              <option value={60}>60 秒</option>
            </select>
          </label>
          <label className="field">
            <span>历史保留时长</span>
            <select
              value={draft.historyRetentionDays}
              onChange={(event) =>
                setDraft({
                  ...draft,
                  historyRetentionDays: Number(event.target.value),
                })
              }
            >
              <option value={7}>7 天</option>
              <option value={30}>30 天</option>
              <option value={90}>90 天</option>
              <option value={365}>1 年</option>
            </select>
            <small>只清理已结束的执行及其用量，运行中和待处理任务会保留。</small>
          </label>
        </section>
        <section className="settings-section">
          <div className="section-title">
            <Bell size={17} />
            <div>
              <h2>应用偏好</h2>
              <p>新主任务事件提醒，历史回填保持静默。</p>
            </div>
          </div>
          <Toggle
            label="桌面提醒"
            checked={draft.notificationsEnabled}
            onChange={(notificationsEnabled) =>
              setDraft({ ...draft, notificationsEnabled })
            }
          />
          <label className="field notification-duration-field">
            <span>浮窗保留时长</span>
            <div className="number-with-unit">
              <input
                type="number"
                min={1}
                max={60}
                step={1}
                value={durationMinutes}
                aria-label="浮窗保留时长"
                aria-invalid={!durationValid}
                onChange={(event) => setDurationMinutes(event.target.value)}
              />
              <span>分钟</span>
            </div>
            {!durationValid && (
              <small className="field-error">请输入 1–60 之间的整数</small>
            )}
          </label>
          <Toggle
            label="登录后自动启动"
            checked={draft.launchAtLogin}
            onChange={(launchAtLogin) => setDraft({ ...draft, launchAtLogin })}
          />
          <label className="field theme-field">
            <span>外观主题</span>
            <select
              value={draft.theme}
              onChange={(event) =>
                setDraft({
                  ...draft,
                  theme: event.target.value as Settings["theme"],
                })
              }
            >
              <option value="system">跟随系统（默认）</option>
              <option value="light">浅色</option>
              <option value="dark">深色</option>
            </select>
          </label>
          <div className="privacy-setting">
            <ShieldCheck size={15} />
            <span>仅保存执行元数据和用量</span>
          </div>
        </section>
      </div>
      <section className="diagnostics-section">
        <div className="section-title">
          <Activity size={17} />
          <div>
            <h2>连接诊断</h2>
          </div>
        </div>
        <dl className="diagnostic-list">
          {(
            [
              ["Codex 状态", diagnostics?.connectionStatus ?? "未读取"],
              ["传输", diagnostics?.transport ?? "未读取"],
              ["Codex 版本", diagnostics?.codexVersion ?? "未读取"],
              [
                "兼容模式",
                diagnostics === null
                  ? "未读取"
                  : diagnostics.compatibilityMode
                    ? "已启用"
                    : "未启用",
              ],
              [
                "快照位置",
                "~/Library/Application Support/Agent Monitor/monitor.db",
              ],
            ] as const
          ).map(([label, value]) => (
            <div className="diagnostic" key={label}>
              <dt>{label}</dt>
              <dd title={value}>{value}</dd>
            </div>
          ))}
        </dl>
        {diagnostics?.degradationReason !== null &&
          diagnostics?.degradationReason !== undefined && (
            <div className="diagnostic-warning">
              <AlertCircle size={15} />
              {diagnostics.degradationReason}
            </div>
          )}
      </section>
    </div>
  );
}
