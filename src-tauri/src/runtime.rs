use crate::activity::{
    ActivityPage, ActivityQuery, ActivityService, ActivitySnapshot, ExecutionRecord,
};
use crate::desktop_notices::{DesktopNotice, DesktopNotices};
use crate::model::NotificationEvent;
use crate::monitor::{MonitorService, MonitorSignal};
use crate::passport::{AlertRecord, CompanionService};
use crate::quota::QuotaService;
use crate::reset_news::ResetNews;
use crate::storage::SnapshotStore;
use notify::{Event, RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;
use tauri::Emitter;

pub struct Services {
    pub activity: Arc<ActivityService>,
    pub quota: Arc<QuotaService>,
    pub news: Arc<ResetNews>,
    pub passport: Arc<CompanionService>,
    pub notices: DesktopNotices,
    pub monitor: MonitorService,
}

impl Services {
    pub fn new(monitor: MonitorService, store: Arc<SnapshotStore>) -> Result<Arc<Self>, String> {
        Self::with_activity(
            monitor,
            store,
            ActivityService::open_default().map_err(|error| error.to_string())?,
        )
    }

    fn with_activity(
        monitor: MonitorService,
        store: Arc<SnapshotStore>,
        activity: ActivityService,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            activity: Arc::new(activity),
            quota: Arc::new(QuotaService::new()?),
            news: Arc::new(ResetNews::new(store.clone())?),
            passport: Arc::new(CompanionService::new(store)?),
            notices: DesktopNotices::default(),
            monitor,
        }))
    }

    fn collect_activity(&self) -> Result<ActivitySnapshot, String> {
        let live = self.monitor.snapshot().map_err(|error| error.to_string())?;
        let settings = self.monitor.settings().map_err(|error| error.to_string())?;
        self.activity
            .sync_with_retention(
                &live.tasks,
                &self
                    .monitor
                    .session_activity()
                    .map_err(|error| error.to_string())?,
                settings.history_retention_days,
            )
            .map_err(|error| error.to_string())
    }

    pub fn sync(&self) -> Result<(ActivitySnapshot, Vec<AlertRecord>), String> {
        let settings = self.monitor.settings().map_err(|error| error.to_string())?;
        let activity = self.collect_activity()?;
        let mut input_alerts = Vec::new();
        let mut consumed = Vec::new();
        for request in self
            .monitor
            .pending_input_requests()
            .map_err(|error| error.to_string())?
        {
            let id = format!("codex:{}:{}", request.session_id, request.turn_id);
            let Some(execution) = self
                .activity
                .execution(&id)
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            consumed.push(request.id.clone());
            if execution.is_child {
                continue;
            }
            input_alerts.push(AlertRecord {
                id: request.id,
                task_id: id,
                kind: "needs_action".into(),
                title: execution.title,
                created_at: chrono::Utc::now().to_rfc3339(),
                unread: true,
            });
        }
        let fresh = self
            .passport
            .observe(&activity, &self.quota.snapshot()?, &input_alerts)?;
        self.passport
            .prune_history(settings.history_retention_days)?;
        self.monitor
            .consume_input_requests(&consumed)
            .map_err(|error| error.to_string())?;
        Ok((activity, fresh))
    }

    pub fn publish_alerts(
        &self,
        app: &tauri::AppHandle,
        fresh: Vec<AlertRecord>,
    ) -> Result<(), String> {
        let settings = self.monitor.settings().map_err(|error| error.to_string())?;
        if !settings.notifications_enabled || fresh.is_empty() {
            return Ok(());
        }
        let notices = self.notices.publish(
            &fresh,
            settings.notification_duration_seconds,
            chrono::Utc::now(),
        )?;
        crate::update_notice_window(app, &notices);
        for alert in fresh {
            let label = match alert.kind.as_str() {
                "completed" => "已完成",
                "failed" => "失败",
                "reset_news" => "重置消息",
                _ => "需要处理",
            };
            crate::emit_notification(
                app,
                NotificationEvent {
                    event_type: alert.kind.clone(),
                    task_id: alert.task_id,
                    turn_id: None,
                    title: format!("{label} · {}", alert.title),
                    body: if alert.kind == "reset_news" {
                        "查看公告与账户额度"
                    } else {
                        "查看执行详情"
                    }
                    .into(),
                },
            );
        }
        Ok(())
    }

    pub fn start(self: &Arc<Self>, app: Option<tauri::AppHandle>, publish_alerts: bool) {
        let services = self.clone();
        let quota_app = app.clone();
        std::thread::Builder::new()
            .name("agent-monitor-quota".into())
            .spawn(move || loop {
                let fresh = services.news.refresh(false).unwrap_or_default();
                if let Ok(snapshot) = services
                    .quota
                    .refresh_with_codex(Some(services.monitor.account_rate_limits()))
                {
                    if let Some(app) = &quota_app {
                        let _ = app.emit("monitor://quota-updated", snapshot);
                    }
                }
                if let Ok(snapshot) = services.news.snapshot() {
                    let _ = services.passport.reset_news(&snapshot);
                }
                let _ = services.passport.notify_external(&fresh);
                if let Some(app) = &quota_app {
                    if let Ok(snapshot) = services.news.snapshot() {
                        let _ = app.emit("monitor://reset-news-updated", snapshot);
                    }
                    let _ = services.publish_alerts(app, fresh);
                }
                std::thread::sleep(Duration::from_secs(60));
            })
            .ok();
        let services = self.clone();
        let (wake_sender, wake_receiver) = mpsc::channel();
        let monitor_events = services.monitor.subscribe();
        let monitor_wake = wake_sender.clone();
        std::thread::Builder::new()
            .name("agent-monitor-activity-wake".into())
            .spawn(move || {
                while let Ok(signal) = monitor_events.recv() {
                    if matches!(
                        signal,
                        MonitorSignal::SnapshotUpdated(_)
                            | MonitorSignal::ConnectionChanged(_)
                            | MonitorSignal::SyncError(_)
                    ) {
                        let _ = monitor_wake.send(());
                    }
                }
            })
            .ok();
        let activity_events_path = dirs::home_dir().map(|home| crate::activity::events_path(&home));
        std::thread::Builder::new()
            .name("agent-monitor-activity".into())
            .spawn(move || {
                let _activity_watcher = activity_events_path.as_ref().and_then(|events_path| {
                    let parent = events_path.parent()?.to_path_buf();
                    if !parent.is_dir() {
                        return None;
                    }
                    let event_file = events_path.clone();
                    let wake = wake_sender.clone();
                    let mut watcher =
                        notify::recommended_watcher(move |result: Result<Event, notify::Error>| {
                            if let Ok(event) = result {
                                if event.paths.iter().any(|path| path == &event_file) {
                                    let _ = wake.send(());
                                }
                            }
                        })
                        .ok()?;
                    watcher.watch(&parent, RecursiveMode::NonRecursive).ok()?;
                    Some(watcher)
                });
                loop {
                    let next_wait;
                    let result = if publish_alerts {
                        services.sync()
                    } else {
                        services
                            .collect_activity()
                            .map(|snapshot| (snapshot, Vec::new()))
                    };
                    match result {
                        Ok((snapshot, fresh)) => {
                            next_wait = activity_wait(&snapshot);
                            if let Some(app) = &app {
                                let _ = app.emit("monitor://activity-updated", &snapshot);
                                if let Ok(state) = services.passport.state() {
                                    let _ = app.emit("monitor://passport-updated", state);
                                }
                                if let Ok(alerts) = services.passport.alerts() {
                                    let _ = app.emit("monitor://alerts-updated", alerts);
                                }
                                if let Some(tray) = app.tray_by_id("main-tray") {
                                    let title = [
                                        (snapshot.summary.running, "运行"),
                                        (snapshot.summary.needs_action, "待办"),
                                    ]
                                    .into_iter()
                                    .filter(|(count, _)| *count > 0)
                                    .map(|(count, label)| format!("{count}{label}"))
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                    let _ = tray.set_title((!title.is_empty()).then_some(title));
                                    let _ = tray.set_tooltip(Some(format!(
                                        "Agent Monitor · 运行 {} · 待处理 {}",
                                        snapshot.summary.running, snapshot.summary.needs_action
                                    )));
                                }
                                if let Err(error) = services.publish_alerts(app, fresh) {
                                    let _ =
                                        app.emit("monitor://sync-error", json!({"message":error}));
                                }
                            }
                        }
                        Err(error) => {
                            next_wait = Duration::from_secs(5);
                            if let Some(app) = &app {
                                let _ = app.emit("monitor://sync-error", json!({"message":error}));
                            }
                        }
                    }
                    if let Some(app) = &app {
                        if let Ok(Some(notices)) = services.notices.expire(chrono::Utc::now()) {
                            crate::update_notice_window(app, &notices);
                        }
                    }
                    match wake_receiver.recv_timeout(next_wait) {
                        Ok(()) => while wake_receiver.try_recv().is_ok() {},
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => std::thread::sleep(next_wait),
                    }
                }
            })
            .ok();
    }

    pub fn command(&self, name: &str, args: &Value) -> Result<Value, String> {
        let value = match name {
            "get_activity_snapshot" => serde_json::to_value(
                self.activity
                    .snapshot()
                    .map_err(|error| error.to_string())?,
            ),
            "get_execution_history" | "get_session_history" => {
                let query = match args.get("query") {
                    Some(value) => serde_json::from_value::<ActivityQuery>(value.clone())
                        .map_err(|error| error.to_string())?,
                    None => ActivityQuery::default(),
                };
                if name == "get_session_history" {
                    serde_json::to_value(
                        self.activity
                            .sessions(query)
                            .map_err(|error| error.to_string())?,
                    )
                } else {
                    serde_json::to_value(
                        self.activity
                            .history(query)
                            .map_err(|error| error.to_string())?,
                    )
                }
            }
            "get_execution_detail" => serde_json::to_value(
                self.activity
                    .execution(args["id"].as_str().ok_or("缺少执行 ID")?)
                    .map_err(|error| error.to_string())?,
            ),
            "get_quota_snapshot" => serde_json::to_value(self.quota.snapshot()?),
            "get_reset_news" => serde_json::to_value(self.news.snapshot()?),
            "refresh_quotas" => serde_json::to_value(self.quota.refresh()?),
            "get_passport_state" => serde_json::to_value(self.passport.state()?),
            "get_alerts" => serde_json::to_value(self.passport.alerts()?),
            "get_desktop_notices" => serde_json::to_value(self.notices.snapshot()?),
            "get_settings" => {
                serde_json::to_value(self.monitor.settings().map_err(|error| error.to_string())?)
            }
            "get_diagnostics" => serde_json::to_value(
                self.monitor
                    .diagnostics()
                    .map_err(|error| error.to_string())?,
            ),
            "sync_now" => {
                self.monitor.sync_now().map_err(|error| error.to_string())?;
                self.collect_activity()?;
                Ok(Value::Null)
            }
            _ => return Err("只读预览不提供该操作，请使用桌面应用".into()),
        };
        value.map_err(|error| error.to_string())
    }
}

fn activity_wait(snapshot: &ActivitySnapshot) -> Duration {
    if snapshot.summary.needs_action > 0 {
        Duration::from_secs(2)
    } else if snapshot.summary.running > 0 {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(15)
    }
}

pub struct RuntimeState(pub Arc<Services>);

#[tauri::command]
pub async fn open_execution(
    id: String,
    state: tauri::State<'_, RuntimeState>,
) -> Result<(), String> {
    let services = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let execution = services
            .activity
            .execution(&id)
            .map_err(|error| error.to_string())?
            .ok_or("该执行记录已不可用")?;
        match execution.client.as_str() {
            "codex" => services
                .monitor
                .open_task(&execution.session_id)
                .map_err(|error| error.to_string()),
            "zcode" => crate::source_navigation::open_zcode_project(&execution.project_path),
            _ => Err("该客户端未提供原应用跳转入口".into()),
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn get_desktop_notices(
    state: tauri::State<'_, RuntimeState>,
) -> Result<Vec<DesktopNotice>, String> {
    state.0.notices.snapshot()
}

#[tauri::command]
pub fn dismiss_desktop_notice(
    id: String,
    state: tauri::State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<Vec<DesktopNotice>, String> {
    let (_, remaining) = state.0.notices.dismiss(&id)?;
    crate::update_notice_window(&app, &remaining);
    Ok(remaining)
}

#[tauri::command]
pub fn open_desktop_notice(
    id: String,
    state: tauri::State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<Vec<DesktopNotice>, String> {
    let (notice, remaining) = state.0.notices.dismiss(&id)?;
    crate::update_notice_window(&app, &remaining);
    crate::show_main_window(&app);
    app.emit_to(
        "main",
        "monitor://open-task-detail",
        json!({"taskId": notice.task_id}),
    )
    .map_err(|error| error.to_string())?;
    Ok(remaining)
}

#[tauri::command]
pub async fn get_activity_snapshot(
    state: tauri::State<'_, RuntimeState>,
) -> Result<ActivitySnapshot, String> {
    let activity = state.0.activity.clone();
    tauri::async_runtime::spawn_blocking(move || {
        activity.snapshot().map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn get_execution_history(
    query: ActivityQuery,
    state: tauri::State<'_, RuntimeState>,
) -> Result<ActivityPage, String> {
    let activity = state.0.activity.clone();
    tauri::async_runtime::spawn_blocking(move || {
        activity.history(query).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn get_session_history(
    query: ActivityQuery,
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::activity::SessionPage, String> {
    let activity = state.0.activity.clone();
    tauri::async_runtime::spawn_blocking(move || {
        activity.sessions(query).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn get_execution_detail(
    id: String,
    state: tauri::State<'_, RuntimeState>,
) -> Result<Option<ExecutionRecord>, String> {
    let activity = state.0.activity.clone();
    tauri::async_runtime::spawn_blocking(move || {
        activity.execution(&id).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn get_quota_snapshot(
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::quota::QuotaSnapshot, String> {
    state.0.quota.snapshot()
}

#[tauri::command]
pub fn get_reset_news(
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::reset_news::NewsSnapshot, String> {
    state.0.news.snapshot()
}

#[tauri::command]
pub fn open_reset_post(
    id: String,
    state: tauri::State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let news = state.0.news.snapshot()?;
    let post = news
        .posts
        .iter()
        .find(|p| p.id == id)
        .ok_or("找不到这条原帖")?;
    app.opener()
        .open_url(&post.url, None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_reset_news(
    settings: crate::reset_news::NewsSettings,
    state: tauri::State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<crate::reset_news::NewsSnapshot, String> {
    let value = state.0.news.settings(settings)?;
    state.0.passport.reset_news(&value)?;
    let _ = app.emit("monitor://reset-news-updated", &value);
    Ok(value)
}

#[tauri::command]
pub async fn refresh_reset_news(
    state: tauri::State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<crate::reset_news::NewsSnapshot, String> {
    let services = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let fresh = services.news.refresh(true)?;
        let value = services.news.snapshot()?;
        services.passport.reset_news(&value)?;
        services.passport.notify_external(&fresh)?;
        services.publish_alerts(&app, fresh)?;
        let _ = app.emit("monitor://reset-news-updated", &value);
        Ok(value)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn refresh_quotas(
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::quota::QuotaSnapshot, String> {
    let services = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        services
            .quota
            .refresh_with_codex(Some(services.monitor.account_rate_limits()))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn get_passport_state(
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::passport::PassportState, String> {
    state.0.passport.state()
}

#[tauri::command]
pub fn list_passport_usb_ports() -> Result<Vec<crate::passport::DeviceTarget>, String> {
    crate::passport::usb_ports()
}

#[tauri::command]
pub async fn scan_passport_ble() -> Result<Vec<crate::passport::DeviceTarget>, String> {
    tauri::async_runtime::spawn_blocking(crate::passport::scan_ble)
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn connect_passport(
    transport: String,
    target: String,
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::passport::PassportState, String> {
    state.0.passport.connect(&transport, &target)
}

#[tauri::command]
pub fn disconnect_passport(
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::passport::PassportState, String> {
    state.0.passport.disconnect()
}

#[tauri::command]
pub fn update_passport_settings(
    settings: crate::passport::PassportSettings,
    state: tauri::State<'_, RuntimeState>,
) -> Result<crate::passport::PassportState, String> {
    state.0.passport.settings(settings)
}

#[tauri::command]
pub fn get_alerts(state: tauri::State<'_, RuntimeState>) -> Result<Vec<AlertRecord>, String> {
    state.0.passport.alerts()
}

#[tauri::command]
pub fn acknowledge_alert(id: String, state: tauri::State<'_, RuntimeState>) -> Result<(), String> {
    state.0.passport.acknowledge(&id)
}

fn preview(services: Arc<Services>, port: u16) -> Result<(), String> {
    let server = tiny_http::Server::http((std::net::Ipv4Addr::LOCALHOST, port))
        .map_err(|error| error.to_string())?;
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist");
    if !dist.join("index.html").is_file() {
        return Err("请先执行 pnpm build".into());
    }
    services.monitor.start_background();
    services.start(None, false);
    println!("Agent Monitor local preview: http://127.0.0.1:{port}/?preview=local");
    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or("").to_owned();
        let response = if path == "/api/command" && request.method() == &tiny_http::Method::Post {
            let mut body = String::new();
            let read = request.as_reader().take(65537).read_to_string(&mut body);
            let result = if read.is_err() || body.len() > 65536 {
                Err("请求过大".into())
            } else {
                serde_json::from_str::<Value>(&body)
                    .map_err(|_| "请求格式错误".to_string())
                    .and_then(|value| {
                        let name = value["command"].as_str().ok_or("缺少命令")?;
                        services.command(name, &value["args"])
                    })
            };
            let (status, data) = match result {
                Ok(value) => (200, value),
                Err(error) => (400, json!({"error":error})),
            };
            tiny_http::Response::from_data(
                serde_json::to_vec(&data).map_err(|error| error.to_string())?,
            )
            .with_status_code(status)
            .with_header(
                tiny_http::Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                    .unwrap(),
            )
        } else if request.method() == &tiny_http::Method::Get {
            let relative = path.trim_start_matches('/');
            if relative.split('/').any(|segment| segment == "..") {
                tiny_http::Response::from_data(b"Not found".to_vec()).with_status_code(404)
            } else {
                let file = if relative.is_empty() {
                    dist.join("index.html")
                } else {
                    dist.join(relative)
                };
                match std::fs::read(&file) {
                    Ok(data) => {
                        let content_type =
                            match file.extension().and_then(|extension| extension.to_str()) {
                                Some("html") => "text/html; charset=utf-8",
                                Some("js") => "text/javascript; charset=utf-8",
                                Some("css") => "text/css; charset=utf-8",
                                Some("svg") => "image/svg+xml",
                                Some("png") => "image/png",
                                _ => "application/octet-stream",
                            };
                        tiny_http::Response::from_data(data).with_header(
                            tiny_http::Header::from_bytes("Content-Type", content_type).unwrap(),
                        )
                    }
                    Err(_) => {
                        tiny_http::Response::from_data(b"Not found".to_vec()).with_status_code(404)
                    }
                }
            }
        } else {
            tiny_http::Response::from_data(b"Method not allowed".to_vec()).with_status_code(405)
        };
        let _ = request.respond(response);
    }
    Ok(())
}

pub fn cli() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        return false;
    };
    let result: Result<(), String> = match command {
        "--ingest-hook" => (|| {
            if args.len() != 3 {
                return Err("hook 参数无效".into());
            }
            let mut body = String::new();
            std::io::stdin()
                .take(2 * 1024 * 1024 + 1)
                .read_to_string(&mut body)
                .map_err(|_| "无法读取 hook 元数据")?;
            if body.len() > 2 * 1024 * 1024 {
                return Err("hook 数据过大".into());
            }
            let value = serde_json::from_str(&body).map_err(|_| "hook 输入格式无效")?;
            crate::activity::ingest_hook(&args[1], &args[2], value)
                .map_err(|_| "hook 元数据记录失败".into())
        })(),
        "--activity-snapshot" => (|| {
            let activity = ActivityService::open_default().map_err(|error| error.to_string())?;
            let snapshot = activity.sync(&[], &[]).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string(&snapshot).map_err(|error| error.to_string())?
            );
            Ok(())
        })(),
        "--benchmark-activity" => (|| {
            let path = PathBuf::from(args.get(1).ok_or("性能诊断需要独立台账路径")?);
            let cycles = args
                .get(2)
                .ok_or("缺少采样次数")?
                .parse::<u8>()
                .map_err(|_| "采样次数无效")?;
            if !(1..=5).contains(&cycles) || !path.is_file() {
                return Err("性能诊断需要已有台账副本，采样次数为 1–5".into());
            }
            let home = dirs::home_dir().ok_or("无法确定用户目录")?;
            let activity = ActivityService::open(home, path).map_err(|error| error.to_string())?;
            let mut samples = Vec::new();
            for cycle in 0..cycles {
                let began = std::time::Instant::now();
                let snapshot = activity.sync(&[], &[]).map_err(|error| error.to_string())?;
                let sync_ms = began.elapsed().as_secs_f64() * 1000.0;
                let began = std::time::Instant::now();
                activity.snapshot().map_err(|error| error.to_string())?;
                samples.push(json!({"cycle":cycle,"syncMs":sync_ms,
                    "snapshotMs":began.elapsed().as_secs_f64()*1000.0,
                    "rootTurns":snapshot.summary.total_turns,
                    "sources":snapshot.sources.iter().map(|source|json!({"client":source.client,"state":source.state})).collect::<Vec<_>>() }));
            }
            println!("{}", json!({"samples":samples}));
            Ok(())
        })(),
        "--quota-snapshot" => (|| {
            let service = QuotaService::new()?;
            let snapshot = service.refresh()?;
            println!(
                "{}",
                serde_json::to_string(&snapshot).map_err(|error| error.to_string())?
            );
            Ok(())
        })(),
        "--reset-news-snapshot" => (|| {
            let store = Arc::new(SnapshotStore::open_default().map_err(|e| e.to_string())?);
            let service = ResetNews::new(store)?;
            service.refresh(true)?;
            println!(
                "{}",
                serde_json::to_string(&service.snapshot()?).map_err(|e| e.to_string())?
            );
            Ok(())
        })(),
        "--account-quotas" => (|| {
            let store = Arc::new(SnapshotStore::open_in_memory().map_err(|e| e.to_string())?);
            let monitor = MonitorService::new(store).map_err(|e| e.to_string())?;
            monitor.sync_now().map_err(|e| e.to_string())?;
            let quota = QuotaService::new()?;
            let snapshot = quota.refresh_with_codex(Some(monitor.account_rate_limits()))?;
            println!(
                "{}",
                serde_json::to_string(&snapshot).map_err(|e| e.to_string())?
            );
            Ok(())
        })(),
        "--passport-scan" => (|| {
            println!(
                "{}",
                serde_json::to_string(&crate::passport::scan_ble()?)
                    .map_err(|error| error.to_string())?
            );
            Ok(())
        })(),
        "--passport-select" => (|| {
            let transport = args.get(1).ok_or("缺少连接方式")?;
            let target = args.get(2).ok_or("缺少设备标识")?;
            if !matches!(transport.as_str(), "usb" | "ble") || target.is_empty() {
                return Err("设备连接参数无效".into());
            }
            let store = SnapshotStore::open_default().map_err(|error| error.to_string())?;
            store
                .save_passport_connection(Some(&crate::passport::PassportConnection {
                    transport: transport.clone(),
                    target: target.clone(),
                }))
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                json!({"selected":true,"appliesOnNextAppStart":true,"transport":transport,"target":target})
            );
            Ok(())
        })(),
        "--passport-smoke" => (|| {
            let transport = args.get(1).ok_or("缺少连接方式")?;
            let target = args.get(2).ok_or("缺少设备标识")?;
            let seconds = args
                .get(3)
                .ok_or("缺少观察秒数")?
                .parse::<u64>()
                .map_err(|_| "观察秒数无效")?;
            if !(10..=3600).contains(&seconds) {
                return Err("观察秒数需在 10..3600 之间".into());
            }
            let store =
                Arc::new(SnapshotStore::open_in_memory().map_err(|error| error.to_string())?);
            let monitor = MonitorService::new(store.clone()).map_err(|error| error.to_string())?;
            monitor.start_background();
            let services = Services::new(monitor, store)?;
            services.passport.connect(transport, target)?;
            services.start(None, true);
            let began = std::time::Instant::now();
            let mut received = false;
            while began.elapsed() < Duration::from_secs(seconds) {
                std::thread::sleep(Duration::from_secs(2));
                let state = services.passport.state()?;
                received |= state.last_sync_at.is_some();
                println!(
                    "{}",
                    serde_json::to_string(&state).map_err(|error| error.to_string())?
                );
            }
            services.passport.disconnect()?;
            if received {
                Ok(())
            } else {
                Err("观察期间未收到设备快照确认".into())
            }
        })(),
        "--install-monitor-hooks" => {
            crate::hooks::install(&std::env::current_exe().unwrap_or_default())
                .map(|result| println!("{}", result))
        }
        "--preview-server" => (|| {
            let port = args
                .get(1)
                .ok_or("缺少端口")?
                .parse::<u16>()
                .map_err(|_| "端口无效")?;
            let directory = PathBuf::from(args.get(2).ok_or("预览需要独立的台账副本目录")?);
            let activity_path = directory.join("activity.db");
            let monitor_path = directory.join("monitor.db");
            if !activity_path.is_file() || !monitor_path.is_file() {
                return Err("预览目录缺少 activity.db 或 monitor.db 副本".into());
            }
            let home = dirs::home_dir().ok_or("无法确定用户目录")?;
            let activity =
                ActivityService::open(home, activity_path).map_err(|error| error.to_string())?;
            let store =
                Arc::new(SnapshotStore::open(monitor_path).map_err(|error| error.to_string())?);
            let monitor = MonitorService::new(store.clone()).map_err(|error| error.to_string())?;
            preview(Services::with_activity(monitor, store, activity)?, port)
        })(),
        _ => return false,
    };
    if let Err(error) = result {
        eprintln!("{error}");
        // A monitoring hook must never block or alter the Agent's tool execution.
        if command == "--ingest-hook" {
            return true;
        }
        std::process::exit(1);
    }
    true
}
